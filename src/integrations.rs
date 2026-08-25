//! Integrations — bundles of personas, skills, HTTP-API tools, and
//! flow templates that can be enabled/disabled as a unit.
//!
//! A pack is a directory laid out exactly like a project:
//! ```text
//! <pack>/
//!   integration.json
//!   personas/<slug>.json
//!   skills/<slug>.md
//!   api_tools/<name>.json
//!   flow_templates/<slug>.json
//! ```
//! Packs live in `<data>/integrations/<id>/`. The built-in `discord`
//! pack is shipped with the binary as a seed and copied into that directory
//! on first run (see [`crate::seed`]).
//!
//! Enable state is persisted in `<data>/integrations.json`:
//! ```json
//! { "discord": { "enabled": true, "enabled_at": "2026-05-29T..." } }
//! ```
//! Packs default to **disabled**. Pack contents are read-only — the workshop
//! API rejects PUT/DELETE against pack-owned items.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::paths;

// The pack manifest, the ecosystem tag, and the ecosystem check live in the
// shared `metalcraft-packs` spec crate so the agent and the registry parse and
// classify packs identically. Re-exported here so existing
// `crate::integrations::{IntegrationManifest, ECOSYSTEM_TAG, is_ecosystem}` paths
// keep working unchanged.
pub use metalcraft_packs::{ECOSYSTEM_TAG, IntegrationManifest, is_ecosystem};

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
pub struct Integration {
    pub manifest: IntegrationManifest,
    /// Directory containing the pack files (`integration.json`, `personas/`, etc.).
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IntegrationState {
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
impl Integration {
    pub fn personas_dir(&self) -> PathBuf {
        self.root.join("personas")
    }
    pub fn skills_dir(&self) -> PathBuf {
        self.root.join("skills")
    }
    pub fn api_tools_dir(&self) -> PathBuf {
        self.root.join("api_tools")
    }
    pub fn flow_templates_dir(&self) -> PathBuf {
        self.root.join("flow_templates")
    }

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
pub fn find_installed(id: &str) -> Option<Integration> {
    list_installed().into_iter().find(|p| p.manifest.id == id)
}

/// Every integration installed on this pod, in sorted-id order.
///
/// One source, because there is one install path: the content store, holding what
/// the installed agent packs vendor. `<data>/integrations/` — the layout that
/// predates agent packs — is not consulted, and [`crate::seed`] deletes it on boot.
/// A pack that is not vendored by an installed agent pack is not installed.
///
/// Two agent packs vendoring different versions of the same integration is a real
/// state the store supports; [`crate::agent_packs::store::resolve`] picks the
/// highest, and this reports what it picked rather than both.
pub fn list_installed() -> Vec<Integration> {
    let mut ids: Vec<String> = crate::agent_packs::store::live_refs()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    ids.sort();
    ids.dedup();

    let mut out = Vec::new();
    for id in ids {
        let Some(root) = crate::agent_packs::store::resolve(&id) else {
            continue;
        };
        let manifest_path = root.join("integration.json");
        let content = match std::fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("skipping pack at {}: {e}", root.display());
                continue;
            }
        };
        match serde_json::from_str::<IntegrationManifest>(&content) {
            Ok(manifest) => out.push(Integration { manifest, root }),
            Err(e) => log::warn!("invalid integration.json in {}: {e}", root.display()),
        }
    }
    out.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    out
}

/// Read the on-disk state map, defaulting to empty (all packs disabled).
pub fn load_state() -> HashMap<String, IntegrationState> {
    let path = paths::integrations_state_file();
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&content).unwrap_or_else(|e| {
        log::warn!("integrations.json is malformed, ignoring: {e}");
        HashMap::new()
    })
}

fn save_state(state: &HashMap<String, IntegrationState>) -> std::io::Result<()> {
    let path = paths::integrations_state_file();
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
fn mutate_state<T>(
    f: impl FnOnce(&mut HashMap<String, IntegrationState>) -> T,
) -> Result<T, String> {
    let lock_path = paths::data_dir().join("integrations.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create state dir: {e}"))?;
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

/// Whether a pack's tools are available.
///
/// **An installed pack is always available.** Enable/disable is retired: scoping is
/// structural now — a tool resolves only if some installed agent pack provides it,
/// the persona references it via `Persona.packs`, *and* the active preset declares
/// it. Three declarations, checked where they matter. A fourth, mutable, global flag
/// had nothing left to decide and could only disagree with them.
///
/// Kept as a function (rather than deleted) so the ~20 call sites and the workshop's
/// `enabled` field keep working while the UIs catch up.
///
/// Both install layouts count. An agent pack vendors its integrations into the
/// content store rather than `<data>/integrations/`, so a legacy-directory-only
/// check reports every one of them as missing — which is how delegating to an
/// agent-pack persona failed with "requires integration(s) [...] that are not
/// installed" while that persona's tools were resolving perfectly well through
/// [`agent_pack_layers`].
pub fn is_enabled(id: &str) -> bool {
    crate::agent_packs::store::resolve(id).is_some()
}

/// Record a pack's provenance (`"registry"` / `"embedded"`) without disturbing
/// its enable state. Creates a disabled entry if none exists yet.
pub fn set_source(id: &str, source: &str) -> Result<(), String> {
    let src = source.to_string();
    mutate_state(|state| {
        state
            .entry(id.to_string())
            .and_modify(|s| s.source = Some(src.clone()))
            .or_insert(IntegrationState {
                enabled: false,
                enabled_at: None,
                source: Some(src),
            });
    })
}

// Pack id/slug validation and semver compare live in the shared spec crate so
// the agent and the registry agree. `valid_pack_id` keeps its local name via the
// alias; the id rule is `^[a-z0-9][a-z0-9_-]{0,63}$`.

/// Canonical content hash of an already-installed pack, computed over the files
/// on disk under `<data>/integrations/<id>/`. Matches the registry's
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

/// Iterate enabled packs in deterministic (sorted-id) order. Used by the
/// resolvers below to walk packs when a user-local item isn't found.
/// Every installed pack. See [`is_enabled`] — installed *is* available now.
pub fn installed_integrations() -> Vec<Integration> {
    list_installed()
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
    let mut map: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for pack in installed_integrations() {
        for key in &pack.manifest.requires_env {
            map.entry(key.clone())
                .or_default()
                .push(pack.manifest.id.clone());
        }
    }
    map.into_iter().collect()
}

// ── Resolvers ───────────────────────────────────────────────────────────
//
// Each accepts a user-local path and walks enabled packs as fallback. The
// returned `IntegrationOrigin` lets the caller tag the wire response with
// `pack_id` + `read_only` so the workshop UI can render it correctly.

/// Where a resolved item came from.
#[derive(Debug, Clone)]
pub enum IntegrationOrigin {
    /// Lives under `<data>/personas/` (or skills/, etc.) — user-owned and editable.
    Local,
    /// Lives under an enabled pack's directory — read-only.
    Pack { id: String },
}

impl IntegrationOrigin {
    pub fn pack_id(&self) -> Option<&str> {
        match self {
            IntegrationOrigin::Local => None,
            IntegrationOrigin::Pack { id } => Some(id),
        }
    }
    pub fn is_read_only(&self) -> bool {
        matches!(self, IntegrationOrigin::Pack { .. })
    }
}

/// Resolve a file by extension under a user-local dir, falling back to the
/// same subdir within each enabled pack. The first hit wins (user-local
/// always shadows pack content).
/// The directories an **installed agent pack** contributes for `pack_subdir`.
///
/// Agent packs are the install unit now, so their personas, skills and presets have
/// to resolve through the same layered lookup that integrations always did —
/// otherwise everything an agent pack installs is written to disk and then invisible.
///
/// `api_tools` are special: a vendored integration lives in the content store
/// under its hash, not inside the agent pack, so those layers point at the store.
pub fn agent_pack_layers(pack_subdir: &str) -> Vec<(PathBuf, IntegrationOrigin)> {
    let mut out = Vec::new();
    for p in crate::agent_packs::list() {
        let origin = IntegrationOrigin::Pack { id: p.id.clone() };
        if pack_subdir == "api_tools" {
            for (_, sha) in crate::agent_packs::store::read_refs(&p.id) {
                out.push((
                    crate::agent_packs::store::entry_dir(&sha).join("api_tools"),
                    origin.clone(),
                ));
            }
        } else {
            out.push((PathBuf::from(&p.root).join(pack_subdir), origin.clone()));
        }
    }
    out
}

pub fn resolve_file(
    local_dir: &Path,
    pack_subdir: &str,
    filename: &str,
) -> Option<(PathBuf, IntegrationOrigin)> {
    let local = local_dir.join(filename);
    if local.exists() {
        return Some((local, IntegrationOrigin::Local));
    }
    // Agent packs first: they are the current install unit. Legacy integration
    // packs still resolve behind them until they are migrated away.
    for (dir, origin) in agent_pack_layers(pack_subdir) {
        let candidate = dir.join(filename);
        if candidate.exists() {
            return Some((candidate, origin));
        }
    }
    for pack in installed_integrations() {
        let candidate = pack.root.join(pack_subdir).join(filename);
        if candidate.exists() {
            return Some((
                candidate,
                IntegrationOrigin::Pack {
                    id: pack.manifest.id,
                },
            ));
        }
    }
    None
}

/// Diagnostic for a resolution miss: is `filename` provided by an installed
/// pack that is currently *disabled*? Returns that pack's id so callers can
/// tell the user to enable it instead of reporting a bare "not found".
pub fn disabled_provider(_pack_subdir: &str, _filename: &str) -> Option<String> {
    // Nothing is disabled any more, so a resolution miss is a genuine miss. Kept so
    // `resolve_or_explain`'s shape is unchanged; it now always reports "not found"
    // rather than sending the user to a switch that no longer exists.
    None
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
) -> Result<(PathBuf, IntegrationOrigin), String> {
    if let Some(hit) = resolve_file(local_dir, pack_subdir, filename) {
        return Ok(hit);
    }
    if let Some(pack_id) = disabled_provider(pack_subdir, filename) {
        return Err(format!(
            "{kind} '{slug}' is provided by integration '{pack_id}', which is disabled. \
             Enable it in the workshop (Integration Packs) to use it."
        ));
    }
    Err(format!(
        "{kind} '{slug}' not found in {} or any enabled integration",
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
) -> Vec<(PathBuf, IntegrationOrigin)> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<(PathBuf, IntegrationOrigin)> = Vec::new();

    let mut push = |path: PathBuf, origin: IntegrationOrigin| {
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
                push(path, IntegrationOrigin::Local);
            }
        }
    }
    // Then each installed agent pack.
    for (dir, origin) in agent_pack_layers(pack_subdir) {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().and_then(|x| x.to_str()) == Some(extension) {
                    push(path, origin.clone());
                }
            }
        }
    }
    // Then each enabled pack.
    for pack in installed_integrations() {
        let pack_dir = pack.root.join(pack_subdir);
        if let Ok(entries) = std::fs::read_dir(&pack_dir) {
            let id = pack.manifest.id.clone();
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().and_then(|x| x.to_str()) == Some(extension) {
                    push(path, IntegrationOrigin::Pack { id: id.clone() });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, tags: &[&str]) -> IntegrationManifest {
        IntegrationManifest {
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
        assert!(is_ecosystem(&manifest(
            "metalcraft-notes",
            &[ECOSYSTEM_TAG]
        )));
        assert!(!is_ecosystem(&manifest("github", &[])));
        // A superset of tags still matches on the ecosystem tag.
        assert!(is_ecosystem(&manifest("x", &["other", ECOSYSTEM_TAG])));
        // A different tag does not.
        assert!(!is_ecosystem(&manifest("x", &["metalcraft"])));
    }

    #[test]
    fn manifest_defaults_tags_to_empty_when_absent() {
        // Older/foreign integration.json with no `tags` key must deserialize (not error)
        // and read as "not an ecosystem pack".
        let m: IntegrationManifest = serde_json::from_str(
            r#"{"id":"github","name":"GitHub","description":"","version":"1.0.0"}"#,
        )
        .expect("manifest without tags should parse");
        assert!(m.tags.is_empty());
        assert!(!is_ecosystem(&m));
    }
}
