//! Migrating a pre-agent-pack pod.
//!
//! Own test binary: `paths::data_dir()` caches `METALCRAFT_DATA_DIR` in a `OnceLock`,
//! so two tests in one process share a data dir whatever they set.

use metalcraft_agent::agent_packs::{self, migrate};
use std::fs;
use std::path::PathBuf;

fn write(root: &PathBuf, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

fn pack_json(id: &str, name: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": id, "name": name, "description": format!("{name} integration"),
        "version": "1.7.1", "requires_env": ["METALCRAFT_TOKEN"],
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

fn persona(name: &str, packs: &[&str], skills: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "name": name, "description": format!("{name} persona"),
        "tools": ["read_file", "load_skill"], "packs": packs, "skills": skills,
        "system_prompt": format!("You are {name}."),
    }))
    .unwrap()
}

#[test]
fn legacy_integration_packs_become_agent_packs() {
    let data_dir = std::env::temp_dir().join(format!("mc-migrate-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }
    fs::create_dir_all(&data_dir).unwrap();

    // A pod as it looks before agent packs exist.
    //
    // 1. A pack that carries its own persona and skill — the common case.
    write(&data_dir, "integration_packs/metalcraft-calendar/pack.json",
          &pack_json("metalcraft-calendar", "Metalcraft Calendar"));
    write(&data_dir, "integration_packs/metalcraft-calendar/api_tools/mcal_list.json",
          &api_tool("mcal_list", "GET", "https://calendar.metalcraftai.com/api/v1/calendars"));
    write(&data_dir, "integration_packs/metalcraft-calendar/api_tools/mcal_create.json",
          &api_tool("mcal_create", "POST", "https://calendar.metalcraftai.com/api/v1/events"));
    write(&data_dir, "integration_packs/metalcraft-calendar/personas/calendar-agent.json",
          &persona("Calendar Agent", &["metalcraft-calendar"], &["scheduling"]));
    write(&data_dir, "integration_packs/metalcraft-calendar/skills/scheduling.md",
          b"# Scheduling\nCheck mcal_now first.\n");
    write(&data_dir, "integration_packs/metalcraft-calendar/README.md", b"# Calendar\n");

    // 2. A pack with tools but no persona of its own.
    write(&data_dir, "integration_packs/github/pack.json", &pack_json("github", "GitHub"));
    write(&data_dir, "integration_packs/github/api_tools/gh_user.json",
          &api_tool("gh_user", "GET", "https://api.github.com/user"));

    // 3. A pack whose persona reaches a *second* pack — containment says both must
    //    be vendored, or the wrapper would not install.
    write(&data_dir, "integration_packs/notes/pack.json", &pack_json("notes", "Notes"));
    write(&data_dir, "integration_packs/notes/api_tools/note_list.json",
          &api_tool("note_list", "GET", "https://notes.metalcraftai.com/api/v1/notes"));
    write(&data_dir, "integration_packs/notes/personas/note-taker.json",
          &persona("Note Taker", &["notes", "metalcraft-calendar"], &[]));

    // ── dry run writes nothing ──────────────────────────────────────────────
    let dry = migrate::run(true);
    assert!(dry.dry_run);
    assert_eq!(dry.migrated.len(), 3, "all three should be wrappable: {:?}", dry.failed);
    assert!(dry.failed.is_empty(), "{:?}", dry.failed);
    assert!(agent_packs::list().is_empty(), "a dry run must not install anything");

    // ── the real thing ──────────────────────────────────────────────────────
    let report = migrate::run(false);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.migrated.len(), 3);

    let installed: Vec<String> = agent_packs::list().into_iter().map(|p| p.id).collect();
    assert_eq!(
        installed,
        vec!["github-legacy", "metalcraft-calendar-legacy", "notes-legacy"],
        "each legacy pack gets a wrapper named after it"
    );

    // The calendar pack's own persona and skill were promoted out of it.
    let cal = report.migrated.iter().find(|m| m.integration_pack == "metalcraft-calendar").unwrap();
    assert_eq!(cal.personas, vec!["calendar-agent"]);
    assert_eq!(cal.skills, vec!["scheduling"]);
    assert!(!cal.persona_synthesized);
    let root = agent_packs::pack_dir("metalcraft-calendar-legacy");
    assert!(root.join("personas/calendar-agent.json").is_file());
    assert!(root.join("skills/scheduling.md").is_file());
    assert!(
        !root.join("integration_packs").exists(),
        "the integration pack goes to the content store, not inside the wrapper"
    );

    // A pack with no persona gets one synthesized, named after the pack so it can
    // never collide with another wrapper's.
    let gh = report.migrated.iter().find(|m| m.integration_pack == "github").unwrap();
    assert!(gh.persona_synthesized);
    assert_eq!(gh.personas, vec!["github-agent"]);

    // Containment: the notes wrapper vendored the calendar pack its persona reaches.
    let pack = agent_packs::find("notes-legacy").unwrap();
    let mut vendored: Vec<&str> =
        pack.manifest.provides.integration_packs.iter().map(|p| p.id.as_str()).collect();
    vendored.sort();
    assert_eq!(vendored, vec!["metalcraft-calendar", "notes"]);
    // …and the consent summary spans both, derived from the tools themselves.
    assert_eq!(
        pack.manifest.domains,
        vec!["calendar.metalcraftai.com", "notes.metalcraftai.com"]
    );

    // The shared calendar pack is stored once, referenced twice.
    let counts = agent_packs::store::refcounts();
    assert_eq!(
        counts.values().copied().max(),
        Some(2),
        "calendar is vendored by two wrappers but stored once"
    );

    // ── idempotent ──────────────────────────────────────────────────────────
    let again = migrate::run(false);
    assert!(again.migrated.is_empty(), "re-running must not duplicate");
    assert_eq!(again.already_migrated.len(), 3);
    assert_eq!(agent_packs::list().len(), 3);

    // ── the presets are usable, not just present ────────────────────────────
    let presets = metalcraft_agent::agent_preset::AgentPreset::list_summaries(
        &metalcraft_agent::paths::agent_presets_dir(),
    );
    let slugs: Vec<&str> = presets.iter().map(|p| p.slug.as_str()).collect();
    assert!(slugs.contains(&"metalcraft-calendar-legacy"));
    let loaded = metalcraft_agent::agent_preset::AgentPreset::load(
        "notes-legacy",
        &metalcraft_agent::paths::agent_presets_dir(),
    )
    .expect("a migrated preset must resolve like any other");
    assert_eq!(loaded.default_persona, "note-taker");
    assert!(loaded.integration_packs.contains(&"metalcraft-calendar".to_string()));

    // ── one unwrappable pack must not block the rest ────────────────────────
    // A persona reaching for a pack that isn't installed can't satisfy containment,
    // so its wrapper is impossible. That pack is reported and left alone; the others
    // stay migrated.
    write(&data_dir, "integration_packs/broken/pack.json", &pack_json("broken", "Broken"));
    write(&data_dir, "integration_packs/broken/api_tools/b_get.json",
          &api_tool("b_get", "GET", "https://example.com/x"));
    write(&data_dir, "integration_packs/broken/personas/needy.json",
          &persona("Needy", &["broken", "not-installed"], &[]));

    let mixed = migrate::run(false);
    assert_eq!(mixed.migrated.len(), 0, "the three good ones were already done");
    assert_eq!(mixed.already_migrated.len(), 3);
    assert_eq!(mixed.failed.len(), 1);
    assert_eq!(mixed.failed[0].0, "broken");
    assert!(mixed.failed[0].1.contains("not-installed"), "{}", mixed.failed[0].1);
    assert!(
        agent_packs::find("broken-legacy").is_none(),
        "a pack that could not be wrapped must leave nothing behind"
    );
    assert_eq!(agent_packs::list().len(), 3, "the successful wrappers are untouched");

    let _ = fs::remove_dir_all(&data_dir);
}
