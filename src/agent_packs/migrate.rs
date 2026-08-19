//! Migrating a pre-agent-pack pod.
//!
//! Before agent packs, an integration pack was the install unit and could carry
//! `personas/` and `skills/` of its own. Both of those now belong to an agent pack,
//! and an integration pack is a vendored dependency. This wraps each installed
//! integration pack into a **legacy agent pack** so an upgraded pod keeps everything
//! it had.
//!
//! **It goes through the installer.** Rather than writing files into place, it builds
//! a real `.agentpack` in memory and calls [`super::install`] — so a migration can
//! never produce something the installer would reject, and every guarantee the
//! install path makes (containment, self-containedness, hash pinning) holds for
//! migrated content too. If a pack can't be wrapped validly, that pack is reported
//! and skipped; the rest still migrate.
//!
//! Idempotent: a pack whose legacy agent pack already exists is left alone, so this
//! is safe to re-run and safe to expose as a button.
use std::collections::BTreeMap;

use serde::Serialize;

use crate::paths;

/// The suffix that marks a synthesized wrapper, so it is obvious in a UI which
/// agent packs a human authored and which the migration invented.
const LEGACY_SUFFIX: &str = "-legacy";

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct MigrationReport {
    pub dry_run: bool,
    pub migrated: Vec<MigratedPack>,
    /// Already wrapped by an earlier run.
    pub already_migrated: Vec<String>,
    /// `(integration pack id, why)`. Reported rather than fatal — one unwrappable
    /// pack must not block the rest.
    pub failed: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct MigratedPack {
    pub integration_pack: String,
    pub agent_pack: String,
    pub preset: String,
    pub personas: Vec<String>,
    pub skills: Vec<String>,
    /// True when the pack shipped no personas and one was synthesized for it.
    pub persona_synthesized: bool,
}

/// Wrap every legacy integration pack into an agent pack.
pub fn run(dry_run: bool) -> MigrationReport {
    let mut report =
        MigrationReport { dry_run, migrated: Vec::new(), already_migrated: Vec::new(), failed: Vec::new() };

    let root = paths::integration_packs_dir();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return report;
    };

    let mut ids: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("pack.json").is_file())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    ids.sort();

    for id in ids {
        let agent_pack_id = format!("{id}{LEGACY_SUFFIX}");
        if super::find(&agent_pack_id).is_some() {
            report.already_migrated.push(agent_pack_id);
            continue;
        }
        match wrap(&id, &agent_pack_id, dry_run) {
            Ok(m) => report.migrated.push(m),
            Err(e) => report.failed.push((id, e)),
        }
    }
    report
}

/// Build (and unless `dry_run`, install) one legacy agent pack.
fn wrap(id: &str, agent_pack_id: &str, dry_run: bool) -> Result<MigratedPack, String> {
    let pack_root = paths::integration_packs_dir().join(id);
    let manifest: metalcraft_packs::PackManifest =
        serde_json::from_str(&std::fs::read_to_string(pack_root.join("pack.json")).map_err(
            |e| format!("reading pack.json: {e}"),
        )?)
        .map_err(|e| format!("parsing pack.json: {e}"))?;

    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();

    // 1. The integration pack itself, minus the personas and skills it is no longer
    //    allowed to carry — those move up to the agent pack.
    for (rel, bytes) in read_tree(&pack_root)? {
        if rel.starts_with("personas/") || rel.starts_with("skills/") {
            continue;
        }
        files.insert(format!("integration_packs/{id}/{rel}"), bytes);
    }

    // 2. Its personas, promoted.
    let mut personas: Vec<(String, crate::persona::Persona)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(pack_root.join("personas")) {
        for e in entries.filter_map(|e| e.ok()) {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Some(slug) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            let raw = std::fs::read(&path).map_err(|e| format!("reading {slug}: {e}"))?;
            let persona: crate::persona::Persona = serde_json::from_slice(&raw)
                .map_err(|e| format!("persona '{slug}' does not parse: {e}"))?;
            files.insert(format!("personas/{slug}.json"), raw);
            personas.push((slug, persona));
        }
    }
    personas.sort_by(|a, b| a.0.cmp(&b.0));

    // A pack with no personas of its own still deserves to survive: synthesize a
    // minimal agent for it. Naming it after the pack keeps it collision-free —
    // copying a shared persona like `orchestrator-agent` into every wrapper would
    // make bare-slug lookup ambiguous across them.
    let persona_synthesized = personas.is_empty();
    if persona_synthesized {
        let slug = format!("{id}-agent");
        let persona = crate::persona::Persona {
            name: manifest.name.clone(),
            description: format!("Uses the {} integration.", manifest.name),
            tools: vec!["load_skill".to_string()],
            packs: vec![id.to_string()],
            skills: Vec::new(),
            version: None,
            system_prompt: format!(
                "You are an assistant with access to the {} integration.\n\n{}",
                manifest.name, manifest.description
            ),
        };
        let raw = serde_json::to_vec_pretty(&persona)
            .map_err(|e| format!("serializing synthesized persona: {e}"))?;
        files.insert(format!("personas/{slug}.json"), raw);
        personas.push((slug, persona));
    }

    // 3. Its skills, promoted — plus any skill a promoted persona loads from
    //    elsewhere, or the exported pack would carry a persona that can't load it.
    let mut skills: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(pack_root.join("skills")) {
        for e in entries.filter_map(|e| e.ok()) {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("md") {
                continue;
            }
            let Some(slug) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            let bytes = std::fs::read(&path).map_err(|e| format!("reading skill {slug}: {e}"))?;
            files.insert(format!("skills/{slug}.md"), bytes);
            skills.push(slug);
        }
    }
    for (_, persona) in &personas {
        for s in &persona.skills {
            if skills.contains(s) {
                continue;
            }
            match crate::integration_packs::resolve_file(
                &paths::skills_dir(),
                "skills",
                &format!("{s}.md"),
            ) {
                Some((path, _)) => {
                    let bytes =
                        std::fs::read(&path).map_err(|e| format!("reading skill {s}: {e}"))?;
                    files.insert(format!("skills/{s}.md"), bytes);
                    skills.push(s.clone());
                }
                None => return Err(format!("persona needs skill '{s}', which is not installed")),
            }
        }
    }
    skills.sort();

    // 4. Every *other* integration pack a promoted persona reaches for must be
    //    vendored too, or the containment rule rejects the result.
    let mut required: Vec<String> = vec![id.to_string()];
    for (slug, persona) in &personas {
        for p in &persona.packs {
            if required.contains(p) {
                continue;
            }
            let dir = paths::integration_packs_dir().join(p);
            if !dir.join("pack.json").is_file() {
                return Err(format!(
                    "persona '{slug}' uses integration pack '{p}', which is not installed"
                ));
            }
            for (rel, bytes) in read_tree(&dir)? {
                if rel.starts_with("personas/") || rel.starts_with("skills/") {
                    continue;
                }
                files.insert(format!("integration_packs/{p}/{rel}"), bytes);
            }
            required.push(p.clone());
        }
    }

    // 5. The synthesized preset.
    let preset_slug = agent_pack_id.to_string();
    let default_persona = personas[0].0.clone();
    let preset = crate::agent_preset::AgentPreset {
        manifest_version: 1,
        slug: preset_slug.clone(),
        name: manifest.name.clone(),
        tagline: None,
        description: format!(
            "Migrated from the '{id}' integration pack, which used to be installed on its own."
        ),
        avatar: None,
        default_persona: default_persona.clone(),
        personas: personas
            .iter()
            .enumerate()
            .map(|(i, (slug, p))| crate::agent_preset::PresetPersona {
                slug: slug.clone(),
                role: if i == 0 {
                    crate::agent_preset::PersonaRole::Default
                } else {
                    crate::agent_preset::PersonaRole::Subagent
                },
                description: Some(p.description.clone()),
            })
            .collect(),
        skills: skills.clone(),
        integration_packs: required.clone(),
        memories: None,
        model: None,
        requires_env: manifest.requires_env.clone(),
        version: Some(manifest.version.clone()),
    };
    files.insert(
        format!("agent_presets/{preset_slug}.json"),
        serde_json::to_vec_pretty(&preset).map_err(|e| format!("serializing preset: {e}"))?,
    );

    // 6. Build a real archive and install it, so migrated content is held to the
    //    same standard as anything downloaded.
    let mut m = super::AgentPackManifest::new(
        agent_pack_id.to_string(),
        manifest.name.clone(),
        manifest.version.clone(),
    );
    m.description = manifest.description.clone();
    m.tags = manifest.tags.clone();
    m.presets = vec![preset_slug.clone()];
    m.provides = super::manifest::Provides {
        personas: personas.iter().map(|(s, _)| s.clone()).collect(),
        skills: skills.clone(),
        integration_packs: required
            .iter()
            .map(|p| super::manifest::PackRef {
                id: p.clone(),
                version: manifest.version.clone(),
                content_sha256: None,
                source: None,
            })
            .collect(),
    };

    let bytes = super::bundle::write(m, files)?;
    // Parse it back even on a dry run: the point of a dry run is to learn whether
    // this *would* work, which is only answerable by validating.
    super::Bundle::read(&bytes)
        .map_err(|e| format!("the wrapper would not install: {e}"))?;
    if !dry_run {
        super::install(&bytes, "migration")?;
    }

    Ok(MigratedPack {
        integration_pack: id.to_string(),
        agent_pack: agent_pack_id.to_string(),
        preset: preset_slug,
        personas: personas.into_iter().map(|(s, _)| s).collect(),
        skills,
        persona_synthesized,
    })
}

fn read_tree(root: &std::path::Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.filter_map(|e| e.ok()) {
            let path = e.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(rel) = path.strip_prefix(root) else { continue };
            let bytes =
                std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
            out.push((rel.to_string_lossy().replace('\\', "/"), bytes));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
