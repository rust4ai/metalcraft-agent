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
use std::path::{Path, PathBuf};

use crate::paths;

/// Manifest for a pack — what `pack.json` contains.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackManifest {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub requires_env: Vec<String>,
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
}

/// Lazily-resolved pack content — we keep [`Pack`] cheap and pull files
/// off disk on demand. Disk I/O happens inside the workshop API handlers
/// which are already on a tokio runtime.
impl Pack {
    pub fn personas_dir(&self) -> PathBuf { self.root.join("personas") }
    pub fn skills_dir(&self) -> PathBuf { self.root.join("skills") }
    pub fn api_tools_dir(&self) -> PathBuf { self.root.join("api_tools") }
    pub fn flow_templates_dir(&self) -> PathBuf { self.root.join("flow_templates") }
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
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    std::fs::write(&path, json)
}

pub fn is_enabled(id: &str) -> bool {
    load_state()
        .get(id)
        .map(|s| s.enabled)
        .unwrap_or(false)
}

pub fn set_enabled(id: &str, enabled: bool) -> Result<(), String> {
    // Make sure the pack actually exists before flipping a flag for it.
    if !list_installed().iter().any(|p| p.manifest.id == id) {
        return Err(format!("pack '{id}' not installed"));
    }
    let mut state = load_state();
    state
        .entry(id.to_string())
        .and_modify(|s| {
            s.enabled = enabled;
            s.enabled_at = if enabled {
                Some(chrono::Utc::now().to_rfc3339())
            } else {
                None
            };
        })
        .or_insert(PackState {
            enabled,
            enabled_at: if enabled {
                Some(chrono::Utc::now().to_rfc3339())
            } else {
                None
            },
        });
    save_state(&state).map_err(|e| format!("failed to write state: {e}"))
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
