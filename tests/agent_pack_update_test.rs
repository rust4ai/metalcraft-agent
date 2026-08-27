//! Updating an agent pack: what follows, and what must never be lost.
//!
//! The two cases here are the ones an install alone gets wrong. Both leave an agent
//! pointing at something that no longer exists, and both are silent — the agent's
//! next turn simply fails, days after the update that caused it.

use metalcraft_agent::agent_packs::{self, bundle, manifest::*};
use std::collections::BTreeMap;
use std::fs;

fn persona(slug: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "name": slug,
        "description": format!("{slug} persona"),
        "tools": ["read_file"],
        "packs": [],
        "skills": [],
        "system_prompt": format!("You are {slug}."),
    }))
    .unwrap()
}

fn preset(slug: &str, default: &str, roster: &[&str]) -> Vec<u8> {
    let personas: Vec<_> =
        std::iter::once(serde_json::json!({ "slug": default, "role": "default" }))
            .chain(
                roster
                    .iter()
                    .map(|s| serde_json::json!({ "slug": s, "role": "subagent" })),
            )
            .collect();
    serde_json::to_vec(&serde_json::json!({
        "slug": slug,
        "name": "Amy",
        "description": "chef",
        "default_persona": default,
        "personas": personas,
        "version": "1.0.0",
    }))
    .unwrap()
}

/// v1: preset `amy-kitchen`, default `amy`, roster also has `amy-shopper`.
fn v1() -> (AgentPackManifest, BTreeMap<String, Vec<u8>>) {
    let mut f = BTreeMap::new();
    f.insert(
        "agent_presets/amy-kitchen.json".into(),
        preset("amy-kitchen", "amy", &["amy-shopper"]),
    );
    f.insert(
        "agent_presets/amy-kitchen/memories.jsonl".into(),
        b"{\"kind\":\"Semantic\",\"content\":\"Amy braises.\",\"summary\":\"braise\"}\n".to_vec(),
    );
    f.insert("personas/amy.json".into(), persona("amy"));
    f.insert("personas/amy-shopper.json".into(), persona("amy-shopper"));
    let mut m = AgentPackManifest::new("amy-kitchen-agent", "Amy", "1.0.0");
    m.presets = vec!["amy-kitchen".into()];
    m.provides = Provides {
        personas: vec!["amy".into(), "amy-shopper".into()],
        skills: vec![],
        integrations: vec![],
    };
    (m, f)
}

#[test]
fn an_update_carries_agents_forward_and_says_what_changed() {
    let data_dir = std::env::temp_dir().join(format!("mc-update-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe { std::env::set_var("METALCRAFT_DATA_DIR", &data_dir) };
    fs::create_dir_all(&data_dir).unwrap();

    let (m, f) = v1();
    let archive = bundle::write(m, f).unwrap();
    agent_packs::install(&archive, "bundle").expect("install v1");

    // Two agents: one on the default persona, one that moved to `amy-shopper`.
    let p = metalcraft_agent::agent_preset::AgentPreset::load(
        "amy-kitchen",
        &metalcraft_agent::paths::agent_presets_dir(),
    )
    .expect("preset resolves");
    let mut keeper = metalcraft_agent::agent_instance::AgentInstance::new(
        &p,
        metalcraft_agent::agent_instance::InstanceOrigin::Workshop,
    );
    keeper.name = "Amy at home".into();
    keeper.save().unwrap();

    let mut shopper = metalcraft_agent::agent_instance::AgentInstance::new(
        &p,
        metalcraft_agent::agent_instance::InstanceOrigin::Workshop,
    );
    shopper.name = "Amy shopping".into();
    shopper.persona = "amy-shopper".into();
    shopper.save().unwrap();

    // ── v2 withdraws `amy-shopper` ──────────────────────────────────────────
    let (mut m2, mut f2) = v1();
    m2.version = "2.0.0".into();
    m2.provides.personas = vec!["amy".into()];
    f2.remove("personas/amy-shopper.json");
    f2.insert(
        "agent_presets/amy-kitchen.json".into(),
        preset("amy-kitchen", "amy", &[]),
    );
    let archive2 = bundle::write(m2, f2).unwrap();

    let report = agent_packs::update(&archive2, "bundle").expect("update to v2");
    assert_eq!(report.from_version, "1.0.0");
    assert_eq!(report.to_version, "2.0.0");
    assert_eq!(
        report.personas_fell_back.len(),
        1,
        "{:?}",
        report.personas_fell_back
    );
    let fell = &report.personas_fell_back[0];
    assert_eq!(fell.instance, shopper.id);
    assert_eq!(fell.from, "amy-shopper");
    assert_eq!(fell.to, "amy");
    assert!(
        report.orphaned.is_empty(),
        "the preset survived, so nothing is orphaned"
    );

    // The record says so, rather than the change being invisible.
    let reloaded = metalcraft_agent::agent_instance::load(&shopper.id).unwrap();
    assert_eq!(reloaded.persona, "amy");
    assert_eq!(
        reloaded.persona_fallback_from.as_deref(),
        Some("amy-shopper")
    );
    assert_eq!(
        reloaded.name, "Amy shopping",
        "an update never renames an agent"
    );

    // The untouched agent is untouched.
    let keeper_now = metalcraft_agent::agent_instance::load(&keeper.id).unwrap();
    assert_eq!(keeper_now.persona, "amy");
    assert!(keeper_now.persona_fallback_from.is_none());

    // ── v3 withdraws the preset itself ──────────────────────────────────────
    let mut m3 = AgentPackManifest::new("amy-kitchen-agent", "Amy", "3.0.0");
    m3.presets = vec!["amy-baking".into()];
    m3.provides = Provides {
        personas: vec!["amy".into()],
        skills: vec![],
        integrations: vec![],
    };
    let mut f3 = BTreeMap::new();
    f3.insert(
        "agent_presets/amy-baking.json".into(),
        preset("amy-baking", "amy", &[]),
    );
    f3.insert("personas/amy.json".into(), persona("amy"));
    let archive3 = bundle::write(m3, f3).unwrap();

    let report = agent_packs::update(&archive3, "bundle").expect("update to v3");
    assert_eq!(
        report.orphaned.len(),
        2,
        "both agents used the withdrawn preset"
    );
    assert!(
        report
            .orphaned
            .iter()
            .all(|o| o.agent_preset == "amy-kitchen")
    );

    // Nothing was deleted: somebody's memory and conversations are in there.
    let keeper_now = metalcraft_agent::agent_instance::load(&keeper.id).unwrap();
    assert_eq!(keeper_now.name, "Amy at home");
    assert_eq!(
        keeper_now.orphaned_from.as_deref(),
        Some("amy-kitchen-agent")
    );

    // And it still resolves — the frozen copy landed in the user-local layer, which
    // is top precedence, so every existing call site keeps working unchanged.
    let frozen = metalcraft_agent::agent_preset::AgentPreset::load(
        "amy-kitchen",
        &metalcraft_agent::paths::agent_presets_dir(),
    )
    .expect("an orphaned agent's preset must still resolve");
    assert_eq!(frozen.default_persona, "amy");
    assert!(
        metalcraft_agent::paths::data_dir()
            .join("personas/amy.json")
            .is_file(),
        "the personas it names are frozen alongside it, or the preset resolves to nothing"
    );

    let _ = fs::remove_dir_all(&data_dir);
}
