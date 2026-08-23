//! Every bundled flow template (base + integrations) must parse and pass
//! `metalcraft_flows::validate` — so shipped templates are never broken and stay
//! v2-conformant.

use metalcraft_flows::{SavedFlow, validate};
use std::path::{Path, PathBuf};

fn collect_templates(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_templates(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("json")
            && p.parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                == Some("flow_templates")
        {
            out.push(p);
        }
    }
}

#[test]
fn all_seed_flow_templates_parse_and_validate() {
    let seed = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seed");
    let mut files = Vec::new();
    collect_templates(&seed, &mut files);
    assert!(
        !files.is_empty(),
        "found no flow templates under {}",
        seed.display()
    );

    for f in &files {
        let raw = std::fs::read_to_string(f).unwrap();
        let flow: SavedFlow = serde_json::from_str(&raw)
            .unwrap_or_else(|e| panic!("{}: parse error: {e}", f.display()));
        let errs = validate(&flow);
        assert!(
            errs.is_empty(),
            "{} failed validation: {:?}",
            f.display(),
            errs
        );
    }

    // No v1 templates should remain.
    for f in &files {
        let raw = std::fs::read_to_string(f).unwrap();
        let flow: SavedFlow = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            flow.spec_version,
            "2",
            "{} is not spec_version 2",
            f.display()
        );

        // Templates may legitimately leave `error` handles unwired, but every
        // `{{ref}}` / condition variable must resolve — a dangling-reference
        // warning means the template is broken.
        let dangling: Vec<_> = metalcraft_agent::flow_exec::lint_flow(&flow)
            .into_iter()
            .filter(|w| w.contains("no known source") || w.contains("no upstream node produces"))
            .collect();
        assert!(
            dangling.is_empty(),
            "{} has dangling references: {:?}",
            f.display(),
            dangling
        );
    }
}

/// Every persona a shipped flow template names must be reachable from *some*
/// seeded preset — otherwise the template ships unarmable.
///
/// This is not hypothetical. `morning-brief.json` names `morning-briefer` twice, and
/// an unbound flow resolves to `general-agent`, whose roster does not include it — so
/// arming the one scheduled template the pod ships failed the containment check with
/// a message about a persona the user never chose.
///
/// The fix is *not* to widen the default roster: `morning-briefer` reaches
/// `metalcraft-calendar`, and a persona may only use integrations its preset
/// declares, so putting it in `general-agent` would drag the whole calendar
/// integration into the minimal default agent. Instead the installer binds a flow to
/// a preset that can actually run it (`flow_bindings::bind_to_a_capable_preset`), and
/// this test guarantees such a preset exists for everything we ship.
#[test]
fn every_seed_template_has_a_preset_that_can_arm_it() {
    use metalcraft_agent::agent_preset::AgentPreset;

    let seed = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seed");
    let presets: Vec<AgentPreset> = std::fs::read_dir(seed.join("agent_presets"))
        .expect("seed/agent_presets exists")
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok())
        .collect();
    assert!(
        !presets.is_empty(),
        "no seeded presets found — the seed tree moved?"
    );

    let mut files = Vec::new();
    collect_templates(&seed, &mut files);

    for f in &files {
        let raw = std::fs::read_to_string(f).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let named = metalcraft_agent::agent_packs::bundle::flow_personas(&doc);
        if named.is_empty() {
            continue;
        }
        let capable = presets
            .iter()
            .find(|p| named.iter().all(|n| p.allows_persona(n)));
        assert!(
            capable.is_some(),
            "{} names {named:?}, and no seeded preset can reach all of them. \
             Either add the persona to a preset that declares the integrations it \
             uses, or the template ships unarmable.",
            f.display(),
        );
    }
}

/// …and each persona a seeded preset lists has to actually exist, or the roster
/// entry is a promise the pod cannot keep.
#[test]
fn every_persona_a_seeded_preset_lists_is_shipped() {
    use metalcraft_agent::agent_preset::AgentPreset;

    let seed = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seed");
    for e in std::fs::read_dir(seed.join("agent_presets"))
        .unwrap()
        .flatten()
    {
        if e.path().extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let preset: AgentPreset =
            serde_json::from_str(&std::fs::read_to_string(e.path()).unwrap()).unwrap();
        for p in preset.callable_personas() {
            // Personas ship either standalone or inside the integration whose
            // tools they are built around, and a preset may name either.
            let standalone = seed.join("personas").join(format!("{p}.json"));
            let in_a_pack = std::fs::read_dir(seed.join("integrations"))
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| {
                    e.path()
                        .join("personas")
                        .join(format!("{p}.json"))
                        .is_file()
                });
            assert!(
                standalone.is_file() || in_a_pack,
                "'{}' lists persona '{p}', which nothing under seed/ ships",
                preset.slug
            );
        }
    }
}
