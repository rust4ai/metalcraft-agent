//! Agent presets — what you pick when you start a chat, instead of a persona.
//!
//! A preset is a **composition object over primitives that already exist**: it names a
//! default persona, the roster that persona may call, and the skills and integration
//! packs they need. It stores no prompts and no tools of its own — [`Persona`] already
//! carries `tools`, `packs`, `skills` and `system_prompt`, and `Persona::packs` already
//! means "adopt every tool from these integrations". The preset only decides *which*
//! personas are in play.
//!
//! Presets resolve in layers exactly like personas and skills
//! ([`crate::integrations::list_files_layered`]) — user-local first, then every
//! enabled pack — with one deliberate difference: **a slug provided by two packs is an
//! error, never a silent shadow**. See [`AgentPreset::load`].
//!
//! [`Persona`]: crate::persona::Persona
use std::path::Path;

use serde::{Deserialize, Serialize};

/// What a persona is allowed to do inside a preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PersonaRole {
    /// What a new instance starts as. Exactly one per preset.
    Default,
    /// Offered to `sub_agent`'s persona mode.
    #[default]
    Subagent,
    /// Callable only from inside this preset; never listed in a picker.
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PresetPersona {
    pub slug: String,
    #[serde(default)]
    pub role: PersonaRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A **capability floor**, not a model name.
///
/// A hard `"gpt-5.4"` breaks on a pod that doesn't have it. Declare what the agent
/// needs and let the pod map that onto what it has; `prefer` is a hint, never a
/// requirement.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ModelFloor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_context: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer: Option<String>,
}

/// Reference to the seed memories an agent pack ships with a preset.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MemoriesRef {
    pub file: String,
    #[serde(default)]
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dims: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AgentPreset {
    #[serde(default = "legacy_manifest_version")]
    pub manifest_version: u32,
    pub slug: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,

    pub default_persona: String,
    #[serde(default)]
    pub personas: Vec<PresetPersona>,

    /// **Unrestricted delegation.** Normally a preset's `sub_agent` calls are
    /// confined to [`Self::callable_personas`] — the roster it declared. An
    /// orchestrator-style preset is the exception: its whole job is to hand work to
    /// whichever specialist fits, including personas that arrived later with an agent
    /// pack it has never heard of. With this set, every persona installed on the pod
    /// joins its delegation roster (its own still listed first).
    ///
    /// It is a real widening of what the agent can reach, so it stays opt-in per
    /// preset and is logged when an agent pack ships a preset that claims it.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub delegates_to_any_persona: bool,

    /// **A library, not an agent.** The preset exists to carry a persona roster,
    /// skills and integrations into the pod; nothing about it is startable. No
    /// instance may be minted from it, it is not offered in a picker, and no flow
    /// may be bound to it.
    ///
    /// This is what an agent pack sets when its value is the specialist it ships
    /// rather than a dedicated agent to talk to. Nothing is lost by it: the
    /// personas and skills install exactly as before, and `general-agent`
    /// ([`Self::delegates_to_any_persona`]) reaches every persona on the pod — so
    /// the orchestrator can still hand work to the specialist. What goes away is
    /// the "start a new agent as this" entry that nobody wanted to pick.
    ///
    /// Enforced at the doors that mint an [`AgentInstance`] rather than in
    /// [`Self::load`], because resolving a persona or skill through this preset is
    /// precisely what must keep working.
    ///
    /// [`AgentInstance`]: crate::agent_instance::AgentInstance
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub library: bool,

    #[serde(default)]
    pub skills: Vec<String>,
    /// Reads `integration_packs` too — the pre-0.30 name, still present in every
    /// preset authored before the rename.
    #[serde(default, alias = "integration_packs")]
    pub integrations: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memories: Option<MemoriesRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelFloor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_env: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// The version this agent writes into every preset it saves.
///
/// Aligned with the archive manifest's version (`agent_packs::manifest::MANIFEST_VERSION`)
/// so a spec-2 pack does not contain a document numbered 1 — which read as a mistake
/// often enough to be one. Nothing about the preset *format* changed at 2; the number
/// moved so the two documents stop disagreeing.
pub const PRESET_MANIFEST_VERSION: u32 = 2;

/// What a preset with no `manifest_version` is: written before the field existed, which
/// is every preset on every pod today. Reading them keeps working — the version is
/// upgraded when the preset is next saved, not when it is read, so nothing rewrites a
/// user's files behind their back.
fn legacy_manifest_version() -> u32 {
    1
}

/// The slug used when nothing else is chosen. Seeded on first run.
pub const DEFAULT_PRESET: &str = "general-agent";

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PresetSummary {
    pub slug: String,
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    pub default_persona: String,
    pub persona_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_id: Option<String>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub read_only: bool,
    /// Not startable — see [`AgentPreset::library`]. Summaries still carry library
    /// presets (installing over one is still a slug collision, and the UI has a
    /// reason to show what a pack brought); a picker filters on this.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub library: bool,
}

impl AgentPreset {
    /// Load a preset by slug: user-local first, then enabled packs.
    ///
    /// **Ambiguity is an error.** If two enabled packs both provide `research`, this
    /// returns an error naming both qualified ids rather than picking one — a silent
    /// shadow is how you end up talking to an agent you didn't install.
    pub fn load(slug: &str, presets_dir: &Path) -> Result<Self, String> {
        let filename = format!("{slug}.json");
        let providers = Self::providers(presets_dir, &filename);

        match providers.len() {
            0 => Err(crate::integrations::resolve_or_explain(
                presets_dir,
                "agent_presets",
                &filename,
                "Agent preset",
                slug,
            )
            .err()
            .unwrap_or_else(|| format!("Agent preset '{slug}' not found"))),
            _ => {
                // A user-local copy always wins outright; it is the operator's own file.
                let local = providers
                    .iter()
                    .find(|(_, origin)| origin.pack_id().is_none());
                if let Some((path, _)) = local {
                    return Self::read(path);
                }
                if providers.len() > 1 {
                    let mut ids: Vec<String> = providers
                        .iter()
                        .filter_map(|(_, o)| o.pack_id().map(|p| format!("{p}/{slug}")))
                        .collect();
                    ids.sort();
                    return Err(format!(
                        "Agent preset '{slug}' is ambiguous — provided by {}. \
                         Use a qualified id, or remove one of the packs.",
                        ids.join(" and ")
                    ));
                }
                Self::read(&providers[0].0)
            }
        }
    }

    /// Every provider of `filename`, user-local and pack, without deduplication —
    /// `list_files_layered` hides shadowed entries, which is exactly what we need to
    /// see in order to report ambiguity.
    fn providers(
        presets_dir: &Path,
        filename: &str,
    ) -> Vec<(std::path::PathBuf, crate::integrations::IntegrationOrigin)> {
        use crate::integrations::IntegrationOrigin;
        let mut out = Vec::new();
        let local = presets_dir.join(filename);
        if local.is_file() {
            out.push((local, IntegrationOrigin::Local));
        }
        for (dir, origin) in crate::integrations::agent_pack_layers("agent_presets") {
            let candidate = dir.join(filename);
            if candidate.is_file() {
                out.push((candidate, origin));
            }
        }
        for pack in crate::integrations::installed_integrations() {
            let candidate = pack.root.join("agent_presets").join(filename);
            if candidate.is_file() {
                out.push((
                    candidate,
                    IntegrationOrigin::Pack {
                        id: pack.manifest.id.clone(),
                    },
                ));
            }
        }
        out
    }

    fn read(path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        let mut preset: AgentPreset = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
        if preset.slug.is_empty() {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                preset.slug = stem.to_string();
            }
        }
        preset.validate()?;
        Ok(preset)
    }

    /// Structural checks that do not need the filesystem.
    pub fn validate(&self) -> Result<(), String> {
        if self.manifest_version > PRESET_MANIFEST_VERSION {
            return Err(format!(
                "Agent preset '{}' is manifest_version {}; this agent understands up to {}",
                self.slug, self.manifest_version, PRESET_MANIFEST_VERSION
            ));
        }
        if self.default_persona.trim().is_empty() {
            return Err(format!(
                "Agent preset '{}' has no default_persona",
                self.slug
            ));
        }
        let defaults = self
            .personas
            .iter()
            .filter(|p| p.role == PersonaRole::Default)
            .count();
        if defaults > 1 {
            return Err(format!(
                "Agent preset '{}' declares {defaults} personas with role 'default'; exactly one is allowed",
                self.slug
            ));
        }
        if !self.personas.is_empty()
            && !self.personas.iter().any(|p| p.slug == self.default_persona)
        {
            return Err(format!(
                "Agent preset '{}' names default_persona '{}', which is not in its roster",
                self.slug, self.default_persona
            ));
        }
        Ok(())
    }

    /// Personas a `sub_agent` call may delegate to: the roster minus `internal`,
    /// plus the default (which is always callable).
    pub fn callable_personas(&self) -> Vec<String> {
        let mut out = vec![self.default_persona.clone()];
        for p in &self.personas {
            if p.role != PersonaRole::Internal && !out.contains(&p.slug) {
                out.push(p.slug.clone());
            }
        }
        out
    }

    /// What `sub_agent` may actually delegate to: [`Self::callable_personas`], plus —
    /// when the preset sets [`Self::delegates_to_any_persona`] — every other persona
    /// installed on the pod.
    ///
    /// The declared roster stays at the front, because this list is also the tool
    /// schema's `enum` and the order is what the model reads first: an orchestrator
    /// should still reach for the specialist its own author chose before it goes
    /// shopping in the rest of the pod.
    pub fn delegation_roster(&self, personas_dir: &Path) -> Vec<String> {
        let mut out = self.callable_personas();
        if self.delegates_to_any_persona {
            for slug in crate::persona::Persona::list_available(personas_dir) {
                if !out.contains(&slug) {
                    out.push(slug);
                }
            }
        }
        out
    }

    /// Every integration the agent can actually reach: the ones this preset declares,
    /// plus the ones its callable personas declare for themselves.
    ///
    /// A persona is not confined to its preset's list. It carries `packs` because a
    /// persona *is* the thing built around a set of tools — a github-agent without the
    /// github tools is not a smaller agent, it is a broken one — and a preset that had
    /// to re-declare each one was a second place to forget. So the preset's list is
    /// what the preset itself wants in play, not a ceiling on its roster.
    ///
    /// What still has to hold is **self-containedness**: whatever the personas reach,
    /// the archive vendors, so an exported pack installs with no network and a consent
    /// summary can be computed from the bytes. That is why this union exists — export
    /// and consent are both derived from it, and neither may see less than the agent can.
    pub fn reachable_integrations(&self, personas_dir: &Path) -> Vec<String> {
        let mut out = self.integrations.clone();
        for slug in self.callable_personas() {
            let Ok(persona) = crate::persona::Persona::load(&slug, personas_dir) else {
                continue;
            };
            for id in persona.integrations {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        out
    }

    /// Whether `slug` may be reached from inside this preset at all.
    ///
    /// A preset with [`Self::delegates_to_any_persona`] allows all of them — the
    /// containment question has one answer per preset, and a second one here would
    /// only disagree with the roster `sub_agent` was actually built from.
    pub fn allows_persona(&self, slug: &str) -> bool {
        self.delegates_to_any_persona
            || slug == self.default_persona
            || self.personas.iter().any(|p| p.slug == slug)
    }

    /// Refuse a preset nobody can be. See [`Self::library`].
    ///
    /// Called at every door that mints an instance, so the refusal reaches the
    /// caller as a message about *this* preset rather than as an agent that exists
    /// but has nothing to say.
    pub fn ensure_spawnable(&self) -> Result<(), String> {
        if self.library {
            return Err(format!(
                "Agent preset '{}' is a library: it provides personas and skills for other \
                 agents to use, and no agent can be started as it. Start '{DEFAULT_PRESET}' \
                 instead — it can delegate to every persona installed on this pod.",
                self.slug
            ));
        }
        Ok(())
    }

    pub fn list_available(presets_dir: &Path) -> Vec<String> {
        let mut slugs: Vec<String> =
            crate::integrations::list_files_layered(presets_dir, "agent_presets", "json")
                .into_iter()
                .filter_map(|(path, _)| {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .map(str::to_string)
                })
                .collect();
        slugs.sort();
        slugs.dedup();
        slugs
    }

    pub fn list_summaries(presets_dir: &Path) -> Vec<PresetSummary> {
        crate::integrations::list_files_layered(presets_dir, "agent_presets", "json")
            .into_iter()
            .filter_map(|(path, origin)| {
                let slug = path.file_stem().and_then(|s| s.to_str())?.to_string();
                let content = std::fs::read_to_string(&path).ok()?;
                let p: AgentPreset = serde_json::from_str(&content).ok()?;
                Some(PresetSummary {
                    slug,
                    name: p.name,
                    description: p.description,
                    tagline: p.tagline,
                    default_persona: p.default_persona,
                    persona_count: p.personas.len(),
                    pack_id: origin.pack_id().map(str::to_string),
                    read_only: origin.is_read_only(),
                    library: p.library,
                })
            })
            .collect()
    }

    pub fn save(&self, presets_dir: &Path) -> Result<(), String> {
        self.validate()?;
        std::fs::create_dir_all(presets_dir)
            .map_err(|e| format!("Failed to create {}: {e}", presets_dir.display()))?;
        let path = presets_dir.join(format!("{}.json", self.slug));
        // Whatever came in, what goes out is current. A pre-2 preset is upgraded the
        // first time something saves it rather than by a migration pass, so a pod that
        // never edits a preset never rewrites it.
        let mut current = self.clone();
        current.manifest_version = PRESET_MANIFEST_VERSION;
        let json = serde_json::to_string_pretty(&current)
            .map_err(|e| format!("Failed to serialize preset: {e}"))?;
        std::fs::write(&path, json).map_err(|e| format!("Failed to write {}: {e}", path.display()))
    }

    pub fn delete(slug: &str, presets_dir: &Path) -> Result<(), String> {
        let path = presets_dir.join(format!("{slug}.json"));
        if !path.is_file() {
            return Err(format!(
                "Agent preset '{slug}' is not user-owned (nothing to delete)"
            ));
        }
        std::fs::remove_file(&path).map_err(|e| format!("Failed to delete {}: {e}", path.display()))
    }
}

/// Resolve the persona a session should start as.
///
/// Falls back rather than failing: an unknown or malformed preset must not stop the
/// agent from starting, it must start as the orchestrator and say why.
pub fn resolve_default_persona(preset_slug: Option<&str>, presets_dir: &Path) -> String {
    let slug = preset_slug.unwrap_or(DEFAULT_PRESET);
    match AgentPreset::load(slug, presets_dir) {
        Ok(p) => p.default_persona,
        Err(e) => {
            if preset_slug.is_some() {
                log::warn!("agent preset '{slug}': {e}; falling back to the default persona");
            }
            crate::runtime::DEFAULT_PERSONA.to_string()
        }
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;

    fn preset(json: &str) -> AgentPreset {
        serde_json::from_str(json).expect("preset")
    }

    #[test]
    fn a_preset_with_no_version_reads_as_the_legacy_one() {
        // Every preset on every pod predates the field. Refusing them would break
        // working installs to align a number.
        let p = preset(r#"{"slug":"a","name":"A","default_persona":"x"}"#);
        assert_eq!(p.manifest_version, 1);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn a_version_1_preset_still_validates() {
        let p = preset(r#"{"manifest_version":1,"slug":"a","name":"A","default_persona":"x"}"#);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn a_preset_from_a_newer_agent_is_refused_rather_than_guessed_at() {
        let p = preset(r#"{"manifest_version":99,"slug":"a","name":"A","default_persona":"x"}"#);
        let err = p.validate().unwrap_err();
        assert!(err.contains("understands up to"), "{err}");
    }

    #[test]
    fn saving_upgrades_a_legacy_preset_to_the_current_version() {
        let dir = std::env::temp_dir().join(format!("preset-version-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p = preset(r#"{"manifest_version":1,"slug":"a","name":"A","default_persona":"x"}"#);
        p.save(&dir).expect("save");
        let written = AgentPreset::load("a", &dir).expect("load");
        assert_eq!(written.manifest_version, PRESET_MANIFEST_VERSION);
        // …and the in-memory value the caller holds is untouched.
        assert_eq!(p.manifest_version, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(json: &str) -> AgentPreset {
        serde_json::from_str(json).expect("parse")
    }

    /// A library preset still *resolves* — that is the whole point of keeping it.
    /// What it must not do is mint an agent.
    #[test]
    fn a_library_preset_resolves_but_cannot_be_started() {
        let p = preset(
            r#"{"slug":"metalcraft-packs","name":"Metalcraft Packs","library":true,
                "default_persona":"metalcraft-packs-agent","skills":["metalcraft-packs"],
                "personas":[{"slug":"metalcraft-packs-agent","role":"default"}]}"#,
        );
        assert!(
            p.validate().is_ok(),
            "a library preset is still a valid preset"
        );
        assert_eq!(p.callable_personas(), vec!["metalcraft-packs-agent"]);
        assert!(p.allows_persona("metalcraft-packs-agent"));

        let err = p.ensure_spawnable().unwrap_err();
        assert!(err.contains("metalcraft-packs"), "{err}");
        assert!(err.contains("library"), "{err}");
    }

    /// The default: every preset written before the field existed, and every one
    /// written since that did not ask to be a library.
    #[test]
    fn a_preset_without_the_flag_is_spawnable() {
        let p = preset(r#"{"slug":"a","name":"A","default_persona":"x"}"#);
        assert!(!p.library);
        assert!(p.ensure_spawnable().is_ok());
        // …and round-trips without growing a field nobody set.
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("library"), "{json}");
    }

    /// The picker reads summaries, so the flag has to survive that hop — a library
    /// preset that looks startable in the list is the bug this guards.
    #[test]
    fn summaries_carry_the_library_flag() {
        let dir = std::env::temp_dir().join(format!("preset-library-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        preset(r#"{"slug":"lib","name":"Lib","library":true,"default_persona":"x"}"#)
            .save(&dir)
            .expect("save");
        preset(r#"{"slug":"real","name":"Real","default_persona":"x"}"#)
            .save(&dir)
            .expect("save");

        let summaries = AgentPreset::list_summaries(&dir);
        let lib = summaries.iter().find(|s| s.slug == "lib").expect("listed");
        let real = summaries.iter().find(|s| s.slug == "real").expect("listed");
        assert!(lib.library, "a library preset is still listed, but flagged");
        assert!(!real.library);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roster_defaults_to_subagent_role() {
        let p = preset(
            r#"{"slug":"a","name":"A","default_persona":"x","personas":[{"slug":"x","role":"default"},{"slug":"y"}]}"#,
        );
        assert_eq!(p.personas[1].role, PersonaRole::Subagent);
    }

    #[test]
    fn callable_excludes_internal_and_always_includes_default() {
        let p = preset(
            r#"{"slug":"a","name":"A","default_persona":"x","personas":[
                {"slug":"x","role":"default"},
                {"slug":"y","role":"subagent"},
                {"slug":"z","role":"internal"}]}"#,
        );
        assert_eq!(p.callable_personas(), vec!["x", "y"]);
        assert!(
            p.allows_persona("z"),
            "internal is reachable, just not offered"
        );
        assert!(!p.allows_persona("w"));
    }

    #[test]
    fn default_persona_must_be_in_the_roster() {
        let p = preset(
            r#"{"slug":"a","name":"A","default_persona":"missing","personas":[{"slug":"x","role":"default"}]}"#,
        );
        let err = p.validate().expect_err("should reject");
        assert!(err.contains("not in its roster"), "{err}");
    }

    #[test]
    fn two_defaults_are_rejected() {
        let p = preset(
            r#"{"slug":"a","name":"A","default_persona":"x","personas":[
                {"slug":"x","role":"default"},{"slug":"y","role":"default"}]}"#,
        );
        assert!(p.validate().is_err());
    }

    /// The roster is the enum `sub_agent` offers, so an unrestricted preset has to
    /// actually *list* what arrived with later packs — otherwise the orchestrator
    /// can only reach what its own author happened to know about.
    #[test]
    fn unrestricted_delegation_widens_the_roster_but_keeps_its_own_first() {
        let dir = std::env::temp_dir().join(format!("preset-roster-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        for slug in ["x", "y", "buildr-space-agent"] {
            std::fs::write(
                dir.join(format!("{slug}.json")),
                format!(r#"{{"name":"{slug}","description":"","system_prompt":"","tools":[]}}"#),
            )
            .expect("write persona");
        }

        let scoped = preset(
            r#"{"slug":"a","name":"A","default_persona":"x","personas":[
                {"slug":"x","role":"default"},{"slug":"y","role":"subagent"}]}"#,
        );
        assert_eq!(scoped.delegation_roster(&dir), vec!["x", "y"]);
        assert!(!scoped.allows_persona("buildr-space-agent"));

        let open = preset(
            r#"{"slug":"a","name":"A","default_persona":"x","delegates_to_any_persona":true,
                "personas":[{"slug":"x","role":"default"},{"slug":"y","role":"subagent"}]}"#,
        );
        // `list_available` is layered, so this also picks up whatever packs the pod
        // running the test has installed — the point of the flag. Assert the shape,
        // not an exact list that would depend on the machine.
        let roster = open.delegation_roster(&dir);
        assert_eq!(
            &roster[..2],
            ["x", "y"],
            "declared roster first, then the rest of the pod: {roster:?}"
        );
        assert!(
            roster.contains(&"buildr-space-agent".to_string()),
            "{roster:?}"
        );
        assert!(open.allows_persona("buildr-space-agent"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Off unless asked for, and absent from the JSON when off — a flag this
    /// consequential should not appear in every preset a user saves.
    #[test]
    fn unrestricted_delegation_is_opt_in_and_not_serialized_when_off() {
        let p = preset(r#"{"slug":"a","name":"A","default_persona":"x"}"#);
        assert!(!p.delegates_to_any_persona);
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(!json.contains("delegates_to_any_persona"), "{json}");
    }

    #[test]
    fn an_empty_roster_is_valid_and_callable_is_just_the_default() {
        let p = preset(r#"{"slug":"a","name":"A","default_persona":"orchestrator-agent"}"#);
        p.validate().expect("valid");
        assert_eq!(p.callable_personas(), vec!["orchestrator-agent"]);
    }
}
