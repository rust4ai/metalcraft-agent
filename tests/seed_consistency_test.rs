//! Every preset this binary ships must be installable.
//!
//! This exists because it wasn't. The seeded `general-agent` listed `morning-briefer`
//! in its roster while declaring no integrations — but that persona declares
//! `packs: ["metalcraft-calendar"]`, so the default agent shipped with every pod
//! violated the containment rule the installer enforces. Nothing caught it: the unit
//! tests build their own fixtures, and the integration tests build archives by hand.
//! It surfaced only when a real pod tried to export and reinstall itself.
//!
//! So: check the actual `seed/` tree, the way the installer would.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn seed_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seed")
}

fn read_json(path: &Path) -> serde_json::Value {
    let raw =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
}

/// Every embedded agent pack's source directory: `seed/agent_packs/<id>/`.
fn seeded_agent_packs() -> Vec<PathBuf> {
    std::fs::read_dir(seed_dir().join("agent_packs"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.join("agent_pack.json").is_file())
                .collect()
        })
        .unwrap_or_default()
}

/// `slug -> (packs it declares, skills it loads)` for every persona the binary ships —
/// the top-level `seed/personas/` **and** those inside agent packs, because a
/// preset resolves personas through the same layered lookup the runtime uses.
fn seeded_personas() -> HashMap<String, (Vec<String>, Vec<String>)> {
    let mut out = HashMap::new();
    let mut dirs = vec![seed_dir().join("personas")];
    dirs.extend(seeded_agent_packs().into_iter().map(|p| p.join("personas")));
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let slug = path.file_stem().unwrap().to_str().unwrap().to_string();
            let v = read_json(&path);
            let strs = |key: &str| -> Vec<String> {
                v.get(key)
                    .and_then(|x| x.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            out.insert(slug, (strs("packs"), strs("skills")));
        }
    }
    out
}

/// Every skill the binary ships — the top-level `seed/skills/` **and** those inside
/// agent packs, because a persona resolves skills through the same layered
/// lookup the runtime uses.
fn seeded_skills() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut dirs = vec![seed_dir().join("skills")];
    dirs.extend(seeded_agent_packs().into_iter().map(|p| p.join("skills")));
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.filter_map(|e| e.ok()) {
            if e.path().extension().and_then(|x| x.to_str()) == Some("md")
                && let Some(stem) = e.path().file_stem().and_then(|s| s.to_str())
            {
                out.push(stem.to_string());
            }
        }
    }
    out
}

/// Every integration the binary ships, which is every one an agent pack vendors.
fn seeded_packs() -> Vec<String> {
    let mut out = Vec::new();
    for pack in seeded_agent_packs() {
        let Ok(entries) = std::fs::read_dir(pack.join("integrations")) else {
            continue;
        };
        for e in entries.filter_map(|e| e.ok()) {
            if e.path().join("integration.json").is_file()
                && let Some(id) = e.file_name().to_str()
            {
                out.push(id.to_string());
            }
        }
    }
    out
}

#[test]
fn every_seeded_preset_is_installable() {
    let presets_dir = seed_dir().join("agent_presets");
    let personas = seeded_personas();
    let skills = seeded_skills();
    let packs = seeded_packs();

    let mut problems: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for entry in std::fs::read_dir(&presets_dir)
        .expect("seed/agent_presets")
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        checked += 1;
        let slug = path.file_stem().unwrap().to_str().unwrap().to_string();
        let raw = std::fs::read_to_string(&path).unwrap();
        let preset: metalcraft_agent::agent_preset::AgentPreset = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("seed/agent_presets/{slug}.json does not parse: {e}"));

        // Structural rules the type enforces (one default, default in roster, …).
        if let Err(e) = preset.validate() {
            problems.push(format!("{slug}: {e}"));
        }
        if preset.slug != slug {
            problems.push(format!("{slug}: declares slug '{}'", preset.slug));
        }

        let declared = &preset.integrations;

        for p in preset.callable_personas() {
            let Some((persona_packs, persona_skills)) = personas.get(&p) else {
                problems.push(format!("{slug}: names persona '{p}', which nothing ships"));
                continue;
            };

            // The containment rule the installer enforces — the one that shipped broken.
            for pack in persona_packs {
                if !declared.contains(pack) {
                    problems.push(format!(
                        "{slug}: persona '{p}' uses integration '{pack}', which the preset does not declare"
                    ));
                }
            }
            // The preset need not enumerate what its personas load — export derives
            // the union (agent_packs::export §4). What must hold is that the skill
            // exists somewhere the layered lookup can find it, or the exported pack
            // would carry a persona whose `load_skill` fails on the installing pod.
            for s in persona_skills {
                if !skills.contains(s) {
                    problems.push(format!(
                        "{slug}: persona '{p}' loads skill '{s}', which nothing ships"
                    ));
                }
            }
        }

        for s in &preset.skills {
            if !skills.contains(s) {
                problems.push(format!("{slug}: names skill '{s}', which nothing ships"));
            }
        }
        for pack in declared {
            if !packs.contains(pack) {
                problems.push(format!(
                    "{slug}: declares integration '{pack}', which no seeded agent pack vendors"
                ));
            }
        }
    }

    assert!(
        checked > 0,
        "no seeded presets found — the seed tree moved?"
    );
    assert!(
        problems.is_empty(),
        "seeded presets that would fail to install:\n  - {}",
        problems.join("\n  - ")
    );
}

#[test]
fn seeded_personas_only_reference_skills_that_exist() {
    // Independent of presets: a persona pointing at a missing skill fails at
    // `load_skill` time, which is a runtime error for something checkable now.
    let skills = seeded_skills();
    let mut problems = Vec::new();
    for (slug, (_, persona_skills)) in seeded_personas() {
        for s in persona_skills {
            if !skills.contains(&s) {
                problems.push(format!(
                    "persona '{slug}' loads skill '{s}', which does not exist"
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn seeded_integrations_have_valid_manifests() {
    let dir = seed_dir().join("integrations");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut checked = 0;
    for e in entries.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
        let manifest = e.path().join("integration.json");
        if !manifest.is_file() {
            continue;
        }
        checked += 1;
        let v = read_json(&manifest);
        let id = v["id"].as_str().unwrap_or_default();
        assert_eq!(
            id,
            e.file_name().to_str().unwrap(),
            "integration.json id must match its directory name"
        );
        assert!(
            !v["version"].as_str().unwrap_or_default().is_empty(),
            "{id} has no version"
        );
    }
    assert!(checked > 0, "no seeded integrations found");
}
