//! Exporting an agent that already exists on a pod.
//!
//! **Its own test binary, not another `#[test]` in the install suite.**
//! `paths::data_dir()` caches `METALCRAFT_DATA_DIR` in a `OnceLock`, so two tests in
//! one process silently share one data dir no matter what they set — which is how the
//! install test started finding this one's artifacts.

use metalcraft_agent::agent_packs::{self, Bundle};
use std::fs;

fn persona(slug: &str, packs: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "name": slug,
        "description": format!("{slug} persona"),
        "tools": ["read_file", "load_skill"],
        "packs": packs,
        "skills": ["knife-skills"],
        "system_prompt": format!("You are {slug}."),
    }))
    .unwrap()
}

fn preset(slug: &str, personas: &[(&str, &str)], packs: &[&str]) -> Vec<u8> {
    let roster: Vec<_> = personas
        .iter()
        .map(|(s, role)| serde_json::json!({ "slug": s, "role": role }))
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "slug": slug,
        "name": "Amy's Kitchen Agent",
        "description": "A chef agent",
        "default_persona": personas[0].0,
        "personas": roster,
        "skills": ["knife-skills"],
        "integration_packs": packs,
        "version": "1.4.0",
    }))
    .unwrap()
}

fn api_tool(name: &str, method: &str, url: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "name": name, "description": name, "method": method, "url": url,
        "headers": { "Authorization": "Bearer $METALCRAFT_TOKEN" },
        "parameters": { "type": "object", "properties": {} },
    }))
    .unwrap()
}

fn pack_manifest(id: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": id, "name": id, "description": "t", "version": "1.7.1",
        "requires_env": ["METALCRAFT_TOKEN"],
    }))
    .unwrap()
}

/// The authoring loop: build an agent out of local personas and skills, package it,
/// then install it as if it had come from somewhere else.
#[test]
fn a_locally_authored_agent_exports_and_reinstalls() {
    let data_dir = std::env::temp_dir().join(format!("mc-ap6-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    let write = |rel: &str, bytes: &[u8]| {
        let p = data_dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    };

    // A pod with a hand-authored agent: a preset, two personas, a skill, and one
    // integration pack in the legacy location.
    write("agent_presets/amy-kitchen.json",
          &preset("amy-kitchen", &[("amy", "default"), ("amy-shopper", "subagent")], &["metalcraft-calendar"]));
    write("agent_presets/amy-kitchen/memories.jsonl",
          b"{\"kind\":\"Semantic\",\"content\":\"Amy braises at 2:1.\",\"summary\":\"ratio\"}\n");
    write("personas/amy.json", &persona("amy", &["metalcraft-calendar"]));
    write("personas/amy-shopper.json", &persona("amy-shopper", &["metalcraft-calendar"]));
    write("skills/knife-skills.md", b"# Knife skills\n");
    write("integration_packs/metalcraft-calendar/pack.json", &pack_manifest("metalcraft-calendar"));
    write("integration_packs/metalcraft-calendar/api_tools/mcal_list.json",
          &api_tool("mcal_list", "GET", "https://calendar.metalcraftai.com/api/v1/calendars"));
    // An old pack that still carries a persona: the export must not smuggle it back in.
    write("integration_packs/metalcraft-calendar/personas/legacy.json", &persona("legacy", &[]));

    let bytes = agent_packs::export("amy-kitchen", "2.0.0").expect("export");

    let parsed = Bundle::read(&bytes).expect("what we exported must be installable");
    assert_eq!(parsed.manifest.id, "amy-kitchen-agent");
    assert_eq!(parsed.manifest.version, "2.0.0");
    assert_eq!(parsed.manifest.presets, vec!["amy-kitchen"]);
    assert!(parsed.files.contains_key("personas/amy-shopper.json"), "the roster travels with it");
    assert!(parsed.files.contains_key("skills/knife-skills.md"));
    assert!(parsed.files.contains_key("agent_presets/amy-kitchen/memories.jsonl"));
    assert!(
        !parsed.files.keys().any(|k| k.contains("integration_packs/metalcraft-calendar/personas/")),
        "a legacy pack's personas must not ride along; presets curate personas now"
    );
    // Each vendored pack is pinned by content, so tampering is caught downstream.
    assert!(parsed.manifest.provides.integration_packs[0].content_sha256.is_some());
    assert_eq!(parsed.consent.domains, vec!["calendar.metalcraftai.com"]);

    // Exporting something the pod can't satisfy fails loudly, here, rather than
    // producing an archive that breaks on someone else's machine.
    write("agent_presets/broken.json",
          &preset("broken", &[("nobody", "default")], &["nonexistent-pack"]));
    let err = agent_packs::export("broken", "0.1.0").expect_err("must refuse");
    assert!(err.contains("persona 'nobody'"), "{err}");
    assert!(err.contains("nonexistent-pack"), "{err}");

    // And the round trip installs.
    let report = agent_packs::install(&bytes, "bundle").expect("install exported pack");
    assert_eq!(report.version, "2.0.0");
    assert_eq!(report.personas.len(), 2);
    assert_eq!(report.memories_indexed, 1);

    let _ = fs::remove_dir_all(&data_dir);
}
