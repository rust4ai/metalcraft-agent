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
    #[serde(default = "one")]
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

fn one() -> u32 {
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
                let local = providers.iter().find(|(_, origin)| origin.pack_id().is_none());
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
                out.push((candidate, IntegrationOrigin::Pack { id: pack.manifest.id.clone() }));
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
        if self.default_persona.trim().is_empty() {
            return Err(format!("Agent preset '{}' has no default_persona", self.slug));
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
        if !self.personas.is_empty() && !self.personas.iter().any(|p| p.slug == self.default_persona)
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

    /// Whether `slug` may be reached from inside this preset at all.
    pub fn allows_persona(&self, slug: &str) -> bool {
        slug == self.default_persona || self.personas.iter().any(|p| p.slug == slug)
    }

    pub fn list_available(presets_dir: &Path) -> Vec<String> {
        let mut slugs: Vec<String> =
            crate::integrations::list_files_layered(presets_dir, "agent_presets", "json")
                .into_iter()
                .filter_map(|(path, _)| {
                    path.file_stem().and_then(|s| s.to_str()).map(str::to_string)
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
                })
            })
            .collect()
    }

    pub fn save(&self, presets_dir: &Path) -> Result<(), String> {
        self.validate()?;
        std::fs::create_dir_all(presets_dir)
            .map_err(|e| format!("Failed to create {}: {e}", presets_dir.display()))?;
        let path = presets_dir.join(format!("{}.json", self.slug));
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize preset: {e}"))?;
        std::fs::write(&path, json).map_err(|e| format!("Failed to write {}: {e}", path.display()))
    }

    pub fn delete(slug: &str, presets_dir: &Path) -> Result<(), String> {
        let path = presets_dir.join(format!("{slug}.json"));
        if !path.is_file() {
            return Err(format!("Agent preset '{slug}' is not user-owned (nothing to delete)"));
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
mod tests {
    use super::*;

    fn preset(json: &str) -> AgentPreset {
        serde_json::from_str(json).expect("parse")
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
        assert!(p.allows_persona("z"), "internal is reachable, just not offered");
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

    #[test]
    fn an_empty_roster_is_valid_and_callable_is_just_the_default() {
        let p = preset(r#"{"slug":"a","name":"A","default_persona":"orchestrator-agent"}"#);
        p.validate().expect("valid");
        assert_eq!(p.callable_personas(), vec!["orchestrator-agent"]);
    }
}
