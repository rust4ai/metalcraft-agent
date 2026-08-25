//! Agent packs — the unit of installation.
//!
//! An agent pack carries one agent preset plus **every** persona, skill and
//! integration it needs. Installing it is the only way new capability reaches a
//! pod; integrations stop being an independent install unit and become vendored
//! dependencies (see [`store`]).
//!
//! ```text
//! <data>/agent_packs/<id>/
//!   agent_pack.json
//!   integrations.json      → { "<pack id>": "<store sha256>" }
//!   agent_presets/<slug>.json   + <slug>/memories.jsonl
//!   personas/<slug>.json
//!   skills/<slug>.md
//! <data>/integration_store/<sha256>/   the vendored packs themselves, deduplicated
//! ```
pub mod bundle;
pub mod manifest;
pub mod store;
pub mod tools;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths;
pub use bundle::Bundle;
pub use manifest::{AgentPackManifest, ConsentSummary};

/// An installed agent pack.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct InstalledAgentPack {
    pub id: String,
    pub manifest: AgentPackManifest,
    pub root: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstallState {
    #[serde(default)]
    pub installed: HashMap<String, InstallRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallRecord {
    pub version: String,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    /// `"bundle"` for a local file, or the registry origin it came from.
    #[serde(default)]
    pub source: String,
}

fn state_file() -> PathBuf {
    paths::data_dir().join("agent_packs.json")
}

pub fn load_state() -> InstallState {
    std::fs::read_to_string(state_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(state: &InstallState) -> Result<(), String> {
    let path = state_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let json =
        serde_json::to_string_pretty(state).map_err(|e| format!("serializing state: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("finalizing {}: {e}", path.display()))
}

pub fn pack_dir(id: &str) -> PathBuf {
    paths::agent_packs_dir().join(id)
}

pub fn find(id: &str) -> Option<InstalledAgentPack> {
    let root = pack_dir(id);
    let raw = std::fs::read_to_string(root.join("agent_pack.json")).ok()?;
    let manifest: AgentPackManifest = serde_json::from_str(&raw).ok()?;
    Some(InstalledAgentPack {
        id: manifest.id.clone(),
        manifest,
        root: root.display().to_string(),
    })
}

/// Every installed agent pack. A malformed one is skipped with a warning — one bad
/// directory must not hide the rest.
pub fn list() -> Vec<InstalledAgentPack> {
    let Ok(entries) = std::fs::read_dir(paths::agent_packs_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<InstalledAgentPack> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let id = e.file_name().to_str()?.to_string();
            match find(&id) {
                Some(p) => Some(p),
                None => {
                    log::warn!("agent pack '{id}' has no readable agent_pack.json; skipping");
                    None
                }
            }
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// What an install actually did. Every field here is something a human or a UI has
/// a reason to see — silence about a skipped dependency is how you get an agent that
/// half works.
#[derive(Debug, Clone, Default, Serialize, utoipa::ToSchema)]
pub struct InstallReport {
    pub id: String,
    pub version: String,
    pub presets: Vec<String>,
    pub personas: Vec<String>,
    pub skills: Vec<String>,
    /// Packs newly written to the content store.
    pub packs_stored: Vec<String>,
    /// Packs already present under the same hash — free, because content-addressed.
    pub packs_deduplicated: Vec<String>,
    /// Credentials the pod does not have yet. A **warning**, never a failure: the
    /// pack installs, its tools error clearly at call time, and `key_set` fixes it.
    pub missing_env: Vec<String>,
    /// Preset slugs already provided by another installed pack.
    pub preset_collisions: Vec<String>,
    pub memories_indexed: usize,
    /// Flows the pack shipped. **Every one lands disabled and unarmed** — installing
    /// an identity must not start background work, and arming is the separate consent
    /// moment that mints the agent to run it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flows_installed_unscheduled: Vec<String>,
    pub consent: ConsentSummary,
}

/// Whether every installed agent pack's manifest and refs parse.
///
/// `gc` deletes store entries no installed pack references, and both `list()` and
/// `read_refs` degrade a parse failure to *nothing* rather than an error. A pack with
/// a truncated `agent_pack.json` therefore contributes zero refs, and collecting
/// would delete the integrations it actually depends on. Skipping is the safe
/// direction: the cost is disk, and the alternative is silent data loss.
fn manifests_all_readable() -> bool {
    let Ok(entries) = std::fs::read_dir(paths::agent_packs_dir()) else {
        return true; // nothing installed
    };
    for e in entries.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
        let manifest = e.path().join("agent_pack.json");
        if !manifest.is_file() {
            return false;
        }
        let ok = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|s| serde_json::from_str::<AgentPackManifest>(&s).ok())
            .is_some();
        if !ok {
            return false;
        }
    }
    true
}

/// Serialises install and uninstall against each other.
///
/// Both mutate the shared content store, and `gc` defines garbage as "not referenced
/// by any installed pack" — a question with a wrong answer for the window between an
/// install writing store entries and recording its refs. A concurrent uninstall's gc
/// landing there deletes content the install is about to depend on, and the install
/// still reports success.
///
/// The API handlers are concurrent (axum) and the agent's own tools call the same
/// functions, so this window is reachable, not theoretical. A separate CLI process
/// sharing the data dir is *not* covered — that needs a file lock, and is a real but
/// much narrower exposure since the two are rarely run together.
fn pack_mutex() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Install an agent pack from `.agentpack` bytes.
///
/// Nothing is written until the archive verifies and validates, so a tampered or
/// incoherent pack leaves the data dir untouched.
pub fn install(bytes: &[u8], source: &str) -> Result<InstallReport, String> {
    // Verify before taking the lock: reading the archive is the expensive part and it
    // touches nothing shared.
    let bundle = Bundle::read(bytes)?;
    let _guard = pack_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let id = bundle.manifest.id.clone();

    // Never downgrade silently.
    if let Some(existing) = find(&id)
        && !version_ge(&bundle.manifest.version, &existing.manifest.version)
    {
        return Err(format!(
            "agent pack '{id}' v{} is older than the installed v{}",
            bundle.manifest.version, existing.manifest.version
        ));
    }

    let mut report = InstallReport {
        id: id.clone(),
        version: bundle.manifest.version.clone(),
        presets: bundle.manifest.presets.clone(),
        consent: bundle.consent.clone(),
        ..Default::default()
    };

    // A slug two packs both provide is reported rather than silently shadowed;
    // resolution errors on ambiguity, so the user needs to know before it bites.
    let existing_presets: Vec<String> =
        crate::agent_preset::AgentPreset::list_summaries(&paths::agent_presets_dir())
            .into_iter()
            .map(|s| s.slug)
            .collect();
    for slug in &bundle.manifest.presets {
        if existing_presets.contains(slug) && find(&id).is_none() {
            report.preset_collisions.push(slug.clone());
        }
    }

    // 1. vendored integrations → the content store
    let mut refs = store::IntegrationRefs::new();
    for (pack_id, files) in bundle::collect_pack_files(&bundle.files) {
        let existed = {
            let sha = metalcraft_packs::canonical_sha256(
                files.iter().map(|(p, c)| (p.as_str(), c.as_slice())),
            );
            store::entry_dir(&sha).join("integration.json").is_file()
        };
        let sha = store::put(&files)?;
        if existed {
            report.packs_deduplicated.push(pack_id.clone());
        } else {
            report.packs_stored.push(pack_id.clone());
        }
        refs.insert(pack_id, sha);
    }

    // 2. the pack's own files
    let root = pack_dir(&id);
    // What the previous version left here, so files the new one dropped can be
    // removed afterwards. Anything the new version no longer ships used to stay
    // behind — personas and presets resolve straight off this directory, so a persona
    // the author withdrew in v2 was still installable after upgrading, and a preset
    // they removed could collide with another pack's and make both unloadable.
    //
    // Written first, deleted after, rather than clearing the directory up front: a
    // reader is not synchronised with the installer, and emptying the root would make
    // an in-flight turn lose its agent's personas, presets and tools mid-turn. This
    // way every path that existed before still resolves throughout, and only the
    // genuinely withdrawn ones stop.
    let previous: std::collections::HashSet<String> = read_tree(&root)
        .unwrap_or_default()
        .into_iter()
        .map(|(rel, _)| rel)
        .collect();
    for (rel, bytes) in &bundle.files {
        if rel.starts_with("integrations/") {
            continue; // lives in the store, not here
        }
        let target = root.join(rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, bytes).map_err(|e| format!("writing {}: {e}", target.display()))?;
        if let Some(p) = rel
            .strip_prefix("personas/")
            .and_then(|p| p.strip_suffix(".json"))
        {
            report.personas.push(p.to_string());
        }
        if let Some(s) = rel
            .strip_prefix("skills/")
            .and_then(|s| s.strip_suffix(".md"))
        {
            report.skills.push(s.to_string());
        }
    }
    let manifest_json = serde_json::to_string_pretty(&bundle.manifest)
        .map_err(|e| format!("serializing agent_pack.json: {e}"))?;
    std::fs::write(root.join("agent_pack.json"), manifest_json)
        .map_err(|e| format!("writing agent_pack.json: {e}"))?;
    store::write_refs(&id, &refs)?;

    // Now retire what this version no longer ships. Everything the new version
    // provides is already in place, so nothing is ever missing in between.
    let shipped: std::collections::HashSet<&str> = bundle
        .files
        .keys()
        .map(String::as_str)
        .filter(|r| !r.starts_with("integrations/"))
        .collect();
    for rel in &previous {
        // Pod-managed files are not "withdrawn content" — the installer writes them
        // itself and no archive ever contains them, so a naive "not in the new
        // archive" test deletes them on every upgrade.
        //
        // For `integrations.json` that was silent and expensive: the refs file
        // is written just above, deleted here, and then `gc` below sees an agent
        // pack referencing nothing and collects every integration it vendors.
        // Upgrading an agent lost all of its tools, with the only trace a store
        // directory that had quietly emptied.
        if is_pod_managed(rel) || shipped.contains(rel.as_str()) {
            continue;
        }
        let stale = root.join(rel);
        if let Err(e) = std::fs::remove_file(&stale) {
            // Leftovers are a correctness problem, not a reason to fail an install
            // that has otherwise landed. Say so loudly enough to notice.
            log::warn!("agent pack '{id}': could not remove withdrawn {rel}: {e}");
        }
    }

    // Collect anything the previous version referenced and this one does not. `gc`
    // used to run only on uninstall, so every upgrade left a full copy of each
    // superseded integration on disk forever.
    //
    // Runs *after* the refs are recorded, never before: garbage is defined as "not
    // referenced by any installed pack", and this pack's new refs have to be visible
    // before that question has the right answer.
    // Only collect when every installed pack could actually be read. `gc` derives
    // liveness from each pack's manifest and refs file, and both fall back to "no
    // refs" on a parse failure — so one corrupt pack would make installing an
    // *unrelated* one delete the corrupt pack's vendored content for good.
    if manifests_all_readable() {
        let freed = store::gc();
        if freed > 0 {
            log::info!("agent pack '{id}': released {freed} superseded store entr(ies)");
        }
    } else {
        log::warn!("agent packs: skipping store cleanup — some installed pack is unreadable");
    }

    // 3. preset memory bases — built once per (preset, version), not per instance
    for slug in &bundle.manifest.presets {
        let seed = root.join("agent_presets").join(slug).join("memories.jsonl");
        if seed.is_file() {
            match crate::memory::instance::build_base(slug, &bundle.manifest.version, &seed) {
                Ok(n) => report.memories_indexed += n,
                // A base that fails to build costs the agent its shipped knowledge,
                // not the install: it still runs, it just starts out knowing less.
                Err(e) => log::warn!("agent pack '{id}': could not build memory base: {e}"),
            }
        }
    }

    // 3b. flows the pack ships — installed, disabled, unarmed, and bound to the
    // pack's preset so the roster check has something to check against.
    if let Some(preset_slug) = bundle.preset_slug() {
        for (rel, bytes) in &bundle.files {
            let Some(name) = rel
                .strip_prefix("flows/")
                .and_then(|f| f.strip_suffix(".json"))
            else {
                continue;
            };
            match install_flow_unscheduled(name, bytes, preset_slug) {
                Ok(id) => report.flows_installed_unscheduled.push(id),
                // A flow that will not parse costs the pack its automation, not its
                // install — the agent itself is still perfectly usable.
                Err(e) => log::warn!("agent pack '{id}': skipping flow '{name}': {e}"),
            }
        }
    }

    // 4. credentials the pod is missing
    for req in &bundle.consent.requires_env {
        // `lookup` already resolves the store *and* the environment, so a key
        // supplied either way counts as present.
        if crate::key_store::lookup(&req.name).is_none() {
            report.missing_env.push(req.name.clone());
        }
    }

    // 5. state + lockfile
    let mut state = load_state();
    state.installed.insert(
        id.clone(),
        InstallRecord {
            version: bundle.manifest.version.clone(),
            installed_at: chrono::Utc::now().to_rfc3339(),
            content_sha256: bundle.manifest.content_sha256.clone(),
            source: source.to_string(),
        },
    );
    save_state(&state)?;

    if let Some(sha) = &bundle.manifest.content_sha256 {
        let _ = crate::lockfile::record_agent_pack(&id, &bundle.manifest.version, sha, source);
    }

    Ok(report)
}

/// What an *installed* preset can reach, derived the same way an install-time
/// summary is: from the vendored integrations' own tool definitions.
///
/// Needed because the second consent moment — arming a flow — happens long after
/// install, and a scheduled agent acts while nobody is watching. "This flow can reach
/// api.instacart.com and 2 of its 9 tools write" is only sayable if it is computed
/// from the bytes at the time you ask, not remembered from a dialog.
pub fn consent_for_preset(preset: &crate::agent_preset::AgentPreset) -> ConsentSummary {
    let mut packs: HashMap<String, (Vec<u8>, Vec<(String, Vec<u8>)>)> = HashMap::new();

    for id in &preset.reachable_integrations(&paths::personas_dir()) {
        let Some(dir) = store::resolve(id) else {
            continue;
        };
        let Ok(manifest) = std::fs::read(dir.join("integration.json")) else {
            continue;
        };
        let mut api_tools = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir.join("api_tools")) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let (Some(name), Ok(bytes)) =
                    (p.file_name().and_then(|n| n.to_str()), std::fs::read(&p))
                else {
                    continue;
                };
                api_tools.push((name.to_string(), bytes));
            }
        }
        api_tools.sort_by(|a, b| a.0.cmp(&b.0));
        packs.insert(id.clone(), (manifest, api_tools));
    }

    manifest::derive_consent(&packs.into_iter().collect())
}

/// One agent whose persona was withdrawn by an update.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PersonaFallback {
    pub instance: String,
    pub name: String,
    /// The persona the new version no longer provides.
    pub from: String,
    /// The preset's new default, which it now uses instead.
    pub to: String,
}

/// One agent whose preset was withdrawn by an update.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Orphaned {
    pub instance: String,
    pub name: String,
    pub agent_preset: String,
    /// Personas and skills copied into the user-local layer so the agent still runs.
    pub frozen: Vec<String>,
}

/// What an update did, beyond what the install did.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct UpdateReport {
    pub id: String,
    pub from_version: String,
    pub to_version: String,
    pub install: InstallReport,
    /// Live agents whose persona was withdrawn and fell back (§12.3).
    pub personas_fell_back: Vec<PersonaFallback>,
    /// Live agents whose preset was withdrawn (§12.3).
    pub orphaned: Vec<Orphaned>,
    /// Agents whose shipped knowledge now points at the new version's base layer.
    pub memory_bases_repointed: Vec<String>,
}

/// Update an installed agent pack, then reconcile the agents made from it.
///
/// **Updating is explicit.** There is no auto-update path into here: nothing changes
/// underneath a running agent because somebody published. That matters more than it
/// sounds — a pack's personas are prompts the model follows and its integrations
/// run on the operator's credentials, so "the author pushed a change" must never be
/// the same event as "your agent started behaving differently".
///
/// Once the operator *has* said yes, live agents follow: prompts, tools, skills,
/// roster and seed memories all resolve against the new version, while learned
/// memories, conversations, names and `persistent` are never touched. The two edge
/// cases are what this function exists for — an install alone would leave an agent
/// pointing at a persona or a preset that no longer exists.
pub fn update(bytes: &[u8], source: &str) -> Result<UpdateReport, String> {
    let bundle = Bundle::read(bytes)?;
    let id = bundle.manifest.id.clone();
    let previous =
        find(&id).ok_or_else(|| format!("agent pack '{id}' is not installed; install it first"))?;
    let from_version = previous.manifest.version.clone();
    let old_presets = previous.manifest.presets.clone();

    // Snapshot the old version's **bytes** before the install replaces them.
    //
    // Paths would not do: the install retires whatever the new version no longer
    // ships, so by the time an orphan needs a frozen copy the file it would be
    // copied from has already been deleted.
    let old_tree: Vec<(String, Vec<u8>)> = read_tree_all(&pack_dir(&id)).unwrap_or_default();

    let install = install(bytes, source)?;

    let mut report = UpdateReport {
        id: id.clone(),
        from_version,
        to_version: bundle.manifest.version.clone(),
        install,
        personas_fell_back: Vec::new(),
        orphaned: Vec::new(),
        memory_bases_repointed: Vec::new(),
    };

    let presets_dir = paths::agent_presets_dir();
    for mut inst in crate::agent_instance::list() {
        if !old_presets.contains(&inst.agent_preset) {
            continue;
        }

        // The preset survived: the agent follows it. Only its persona can have gone.
        if bundle.manifest.presets.contains(&inst.agent_preset) {
            let Ok(preset) =
                crate::agent_preset::AgentPreset::load(&inst.agent_preset, &presets_dir)
            else {
                continue;
            };
            if !preset.allows_persona(&inst.persona) {
                report.personas_fell_back.push(PersonaFallback {
                    instance: inst.id.clone(),
                    name: inst.name.clone(),
                    from: inst.persona.clone(),
                    to: preset.default_persona.clone(),
                });
                inst.persona_fallback_from = Some(inst.persona.clone());
                inst.persona = preset.default_persona.clone();
                if let Err(e) = inst.save() {
                    log::warn!(
                        "update: could not record persona fallback for {}: {e}",
                        inst.id
                    );
                }
            }
        } else {
            // The preset is gone. Never delete the agent — somebody's memory and
            // conversations are in there. Materialize what it needs into the
            // user-local layer, which is top precedence and always resolvable, so
            // every existing call site keeps working without a special case.
            let frozen = freeze_preset(&inst.agent_preset, &old_tree);
            report.orphaned.push(Orphaned {
                instance: inst.id.clone(),
                name: inst.name.clone(),
                agent_preset: inst.agent_preset.clone(),
                frozen,
            });
            inst.orphaned_from = Some(id.clone());
            if let Err(e) = inst.save() {
                log::warn!("update: could not flag orphaned agent {}: {e}", inst.id);
            }
        }

        // Drop it from the resident set so the next turn loads the new base layer.
        // Without this the agent keeps recalling the *old* version's seed memories
        // from an `Arc` nothing else can reach, until the process restarts — the
        // update would appear to have done nothing.
        crate::memory::instance::evict(&inst.id);
        report.memory_bases_repointed.push(inst.id.clone());
    }

    // The old version's base layers have no referents left.
    for slug in &old_presets {
        crate::memory::instance::release_base(slug, &report.from_version);
    }

    Ok(report)
}

/// Copy an orphaned agent's preset — and any persona or skill it names that nothing
/// else provides — into the user-local layer.
///
/// The operator inherits them: they were part of an agent that is still running, and
/// the alternative is a preset that resolves to nothing the first time somebody sends
/// it a message.
fn freeze_preset(slug: &str, old_tree: &[(String, Vec<u8>)]) -> Vec<String> {
    let mut frozen = Vec::new();

    let want = format!("agent_presets/{slug}.json");
    let Some((_, raw)) = old_tree.iter().find(|(p, _)| *p == want) else {
        return frozen;
    };
    let Ok(preset) = serde_json::from_slice::<crate::agent_preset::AgentPreset>(raw) else {
        return frozen;
    };

    let dest = paths::agent_presets_dir().join(format!("{slug}.json"));
    // Never clobber a preset the operator already authored under this slug — theirs
    // wins, and it is already resolvable, which is all the orphan needed.
    if !dest.is_file() {
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&dest, raw).is_ok() {
            frozen.push(format!("agent_presets/{slug}.json"));
        }
    }

    let mut wanted: Vec<String> = preset
        .callable_personas()
        .iter()
        .map(|p| format!("personas/{p}.json"))
        .collect();
    wanted.extend(preset.skills.iter().map(|s| format!("skills/{s}.md")));

    for rel in wanted {
        let Some((_, bytes)) = old_tree.iter().find(|(p, _)| *p == rel) else {
            continue;
        };
        let dest = paths::data_dir().join(&rel);
        if dest.is_file() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&dest, bytes).is_ok() {
            frozen.push(rel);
        }
    }

    frozen
}

/// Files inside `<data>/agent_packs/<id>/` that the pod writes rather than the
/// archive supplying them.
///
/// They must survive an in-place upgrade: they are not part of any version's
/// content, so "the new archive doesn't contain it" says nothing about whether it
/// is still wanted.
fn is_pod_managed(rel: &str) -> bool {
    matches!(rel, "agent_pack.json" | "integrations.json")
}

/// Write one packaged flow to `<data>/flows/`, disabled and unarmed.
///
/// Three things are forced regardless of what the author shipped:
///
/// * `enabled = false` — the master switch. Installing an identity must not start
///   background work, and a pack that arrived with it on would do exactly that.
/// * every `schedules[].enabled = false` — belt and braces, so enabling the flow by
///   hand later still doesn't fire a trigger nobody looked at.
/// * the flow is **bound to the pack's preset**, which is what makes the arm dialog's
///   consent summary constructible: personas, domains and credentials all resolve
///   through the preset that owns the flow.
///
/// A schedule's `instance` is never carried — it lives in `flow_bindings.json`, which
/// this never populates. `arm()` is the only thing that mints an instance.
fn install_flow_unscheduled(name: &str, bytes: &[u8], preset_slug: &str) -> Result<String, String> {
    let mut flow: metalcraft_flows::SavedFlow =
        serde_json::from_slice(bytes).map_err(|e| format!("not a valid flow document: {e}"))?;

    flow.enabled = false;
    for s in &mut flow.schedules {
        s.enabled = false;
    }
    if flow.id.is_empty() {
        flow.id = name.to_string();
    }

    // Don't clobber a flow the operator already has under this id. Their edits are
    // theirs; a pack upgrade that silently reverted them would be indistinguishable
    // from data loss.
    if metalcraft_flows::load_flow(&paths::flows_dir(), &flow.id).is_some() {
        return Err(format!(
            "a flow with id '{}' already exists; leaving it alone",
            flow.id
        ));
    }

    metalcraft_flows::save_flow(&paths::flows_dir(), &flow).map_err(|e| e.to_string())?;
    // Bind after saving: the binding names a flow, and a binding pointing at nothing
    // is worse than no binding.
    if let Err(e) = crate::flow_bindings::bind_preset(&flow, preset_slug) {
        log::warn!(
            "agent pack: saved flow '{}' but could not bind it: {e}",
            flow.id
        );
    }
    Ok(flow.id)
}

/// Uninstall an agent pack.
///
/// **Refuses while a persistent agent still uses one of its presets** — those agents
/// hold memories and conversations, and orphaning them silently is worse than a
/// failed command. `force` orphans them deliberately.
pub fn uninstall(id: &str, force: bool) -> Result<UninstallReport, String> {
    let _guard = pack_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let pack = find(id).ok_or_else(|| format!("agent pack '{id}' is not installed"))?;

    // Every agent made from one of this pack's presets, named or not.
    //
    // The check used to consider only `persistent` ones, which meant an ordinary
    // in-progress chat — unnamed by default — had its preset deleted underneath it
    // with no warning, and its next turn failed to load. Unnamed agents still don't
    // *block* the uninstall (nobody chose to keep them), but they are evicted so a
    // live one stops answering from a preset that no longer exists.
    let all: Vec<crate::agent_instance::AgentInstance> = crate::agent_instance::list()
        .into_iter()
        .filter(|i| pack.manifest.presets.contains(&i.agent_preset))
        .collect();
    let dependents: Vec<String> = all
        .iter()
        .filter(|i| i.persistent)
        .map(|i| format!("{} ({})", i.name, i.id))
        .collect();
    if !dependents.is_empty() && !force {
        return Err(format!(
            "agent pack '{id}' is in use by {} agent(s): {}. Delete them first, or force the uninstall to orphan them.",
            dependents.len(),
            dependents.join(", ")
        ));
    }

    let root = pack_dir(id);
    std::fs::remove_dir_all(&root).map_err(|e| format!("removing {}: {e}", root.display()))?;

    let mut state = load_state();
    state.installed.remove(id);
    save_state(&state)?;
    let _ = crate::lockfile::remove_agent_pack(id);

    // Drop every affected agent from the resident set, and release the memory bases
    // this pack shipped.
    //
    // Without this the removal simply does not take effect for anything already in
    // RAM: a resident instance keeps recalling the uninstalled pack's memories, from
    // an `Arc` nothing else can reach, until the process restarts.
    for inst in &all {
        crate::memory::instance::evict(&inst.id);
    }
    for slug in &pack.manifest.presets {
        crate::memory::instance::release_base(slug, &pack.manifest.version);
    }

    // Only now, once the refs are gone, can the store tell what is unreferenced —
    // and only if every remaining pack is readable, or its content would look like
    // garbage. See `manifests_all_readable`.
    let freed = if manifests_all_readable() {
        store::gc()
    } else {
        0
    };

    Ok(UninstallReport {
        id: id.to_string(),
        orphaned_agents: dependents,
        packs_freed: freed,
    })
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct UninstallReport {
    pub id: String,
    /// Agents left pointing at a preset that no longer exists. Never silently
    /// deleted — someone's memories are in there.
    pub orphaned_agents: Vec<String>,
    pub packs_freed: usize,
}

fn version_ge(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(a), parse(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
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
    fn downgrades_are_refused_but_reinstalls_are_not() {
        assert!(version_ge("1.1.0", "1.0.0"));
        assert!(
            version_ge("1.0.0", "1.0.0"),
            "reinstalling the same version is allowed"
        );
        assert!(!version_ge("1.0.0", "1.1.0"));
    }
}

// ── export ───────────────────────────────────────────────────────────────────

/// Package an agent preset that is already on this pod into a `.agentpack`.
///
/// This is the authoring path: build an agent locally out of personas and skills,
/// then export it as one self-contained file you can install elsewhere or publish.
/// It gathers by *following the preset*, not by copying a directory — so the
/// archive contains exactly what the agent needs and nothing else, and the
/// self-contained rule is satisfied by construction rather than by hope.
///
/// Seed memories come from the preset's `memories.jsonl` if it has one. It does
/// **not** reach into any instance's learned memories: those are the operator's,
/// not the author's.
pub fn export(preset_slug: &str, version: &str) -> Result<Vec<u8>, String> {
    let presets_dir = paths::agent_presets_dir();
    let preset = crate::agent_preset::AgentPreset::load(preset_slug, &presets_dir)?;

    let mut files: std::collections::BTreeMap<String, Vec<u8>> = Default::default();
    let mut missing: Vec<String> = Vec::new();

    // 1. the preset itself
    let preset_json =
        serde_json::to_vec_pretty(&preset).map_err(|e| format!("serializing preset: {e}"))?;
    files.insert(format!("agent_presets/{preset_slug}.json"), preset_json);

    // 2. its seed memories, wherever the preset lives
    for dir in [
        presets_dir.join(preset_slug),
        pack_dir_for_preset(preset_slug),
    ] {
        let seed = dir.join("memories.jsonl");
        if seed.is_file()
            && let Ok(bytes) = std::fs::read(&seed)
        {
            files.insert(format!("agent_presets/{preset_slug}/memories.jsonl"), bytes);
            break;
        }
    }

    // 3. every persona it can call
    let personas_dir = paths::personas_dir();
    let mut persona_names = Vec::new();
    for slug in preset.callable_personas() {
        match crate::persona::Persona::load(&slug, &personas_dir) {
            Ok(p) => {
                let json = serde_json::to_vec_pretty(&p)
                    .map_err(|e| format!("serializing persona '{slug}': {e}"))?;
                files.insert(format!("personas/{slug}.json"), json);
                persona_names.push(slug);
            }
            Err(e) => missing.push(format!("persona '{slug}': {e}")),
        }
    }

    // 4. every skill those personas load
    //
    // The union of what the preset declares and what each persona actually lists —
    // `load_skill`'s enum is built from `persona.skills` (runtime.rs), so a skill a
    // persona names but the preset forgot would be missing from the archive and fail
    // at load time on whoever installed it. Derive it rather than trusting the
    // declaration, the same way the consent summary is derived.
    let skills_dir = paths::skills_dir();
    let mut wanted: Vec<String> = preset.skills.clone();
    for p in preset.callable_personas() {
        if let Ok(persona) = crate::persona::Persona::load(&p, &personas_dir) {
            for s in persona.skills {
                if !wanted.contains(&s) {
                    wanted.push(s);
                }
            }
        }
    }
    let mut skill_names = Vec::new();
    for slug in &wanted {
        match crate::integrations::resolve_file(&skills_dir, "skills", &format!("{slug}.md")) {
            Some((path, _)) => match std::fs::read(&path) {
                Ok(bytes) => {
                    files.insert(format!("skills/{slug}.md"), bytes);
                    skill_names.push(slug.clone());
                }
                Err(e) => missing.push(format!("skill '{slug}': {e}")),
            },
            None => missing.push(format!("skill '{slug}' not found")),
        }
    }

    // 5. every integration the agent can reach — the preset's own plus whatever its
    //    personas declare — vendored, which is what makes the result installable with
    //    no network. Exporting only the preset's list would drop the tools a persona
    //    brought with it, and the pack would install one persona short of working.
    let mut pack_refs = Vec::new();
    for id in &preset.reachable_integrations(&paths::personas_dir()) {
        let Some(dir) = find_pack_dir(id) else {
            missing.push(format!("integration '{id}' is not installed"));
            continue;
        };
        let mut count = 0usize;
        for (rel, bytes) in read_tree(&dir)? {
            files.insert(format!("integrations/{id}/{rel}"), bytes);
            count += 1;
        }
        if count == 0 {
            missing.push(format!("integration '{id}' is empty"));
            continue;
        }
        let version = std::fs::read_to_string(dir.join("integration.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<metalcraft_packs::IntegrationManifest>(&s).ok())
            .map(|m| m.version)
            .unwrap_or_else(|| "0.0.0".to_string());
        pack_refs.push(manifest::IntegrationRef {
            id: id.clone(),
            version,
            content_sha256: None,
            source: None,
        });
    }

    // Refuse rather than ship something that will fail validation on the far side.
    if !missing.is_empty() {
        return Err(format!(
            "cannot export '{preset_slug}' — the pod is missing:\n  - {}",
            missing.join("\n  - ")
        ));
    }

    let mut m = AgentPackManifest::new(
        format!("{preset_slug}-agent"),
        preset.name.clone(),
        version.to_string(),
    );
    m.description = preset.description.clone();
    m.presets = vec![preset_slug.to_string()];
    m.provides = manifest::Provides {
        personas: persona_names,
        skills: skill_names,
        integrations: pack_refs,
    };

    // Pin each vendored pack by content, so a tampered copy is caught at install.
    let by_pack = bundle::collect_pack_files(&files);
    for r in &mut m.provides.integrations {
        if let Some(f) = by_pack.get(&r.id) {
            r.content_sha256 = Some(metalcraft_packs::canonical_sha256(
                f.iter().map(|(p, c)| (p.as_str(), c.as_slice())),
            ));
        }
    }

    let bytes = bundle::write(m, files)?;

    // Read back what we just built. An export that produces an archive the installer
    // would reject is a failure *here*, at authoring time, not on whoever tries to
    // install it. Every caller gets this, not just the ones that remember to check.
    Bundle::read(&bytes)
        .map_err(|e| format!("exported '{preset_slug}', but the result is not installable: {e}"))?;
    Ok(bytes)
}

fn pack_dir_for_preset(preset_slug: &str) -> PathBuf {
    for p in list() {
        if p.manifest.presets.iter().any(|s| s == preset_slug) {
            return pack_dir(&p.id).join("agent_presets").join(preset_slug);
        }
    }
    paths::agent_presets_dir().join(preset_slug)
}

/// An integration's directory: the content store first, then the legacy
/// `<data>/integrations/` layout that predates agent packs.
fn find_pack_dir(id: &str) -> Option<PathBuf> {
    if let Some(dir) = store::resolve(id) {
        return Some(dir);
    }
    let legacy = paths::integrations_dir().join(id);
    legacy.join("integration.json").is_file().then_some(legacy)
}

/// Read a directory tree into `(relative path, bytes)` pairs.
/// Every file under `root`, with archive-style relative paths.
fn read_tree_all(root: &std::path::Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
        for e in entries.filter_map(|e| e.ok()) {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            let bytes =
                std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
            out.push((rel, bytes));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// [`read_tree_all`] minus `personas/` and `skills/`.
///
/// For **export only**: personas and skills moved out of integrations, and an
/// old pack that still carries them must not smuggle them back in through an export.
/// Anything that needs the pack as it actually is on disk — the orphan snapshot, for
/// one — wants [`read_tree_all`] instead.
fn read_tree(root: &std::path::Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    Ok(read_tree_all(root)?
        .into_iter()
        .filter(|(rel, _)| !rel.starts_with("personas/") && !rel.starts_with("skills/"))
        .collect())
}
