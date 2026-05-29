//! Regression test for the bug where flow runs failed with
//! "Persona '<slug>' not found" because the persona lived in an integration
//! pack rather than the local `personas/` dir. Runtime persona loading must
//! resolve enabled packs as a fallback, and report an actionable error when
//! the providing pack is installed but disabled.
//!
//! Everything runs inside ONE `#[test]` so the process-global
//! `METALCRAFT_DATA_DIR` env var isn't raced by parallel tests.

use metalcraft_agent::persona::Persona;
use std::fs;
use std::path::PathBuf;

fn write(path: &PathBuf, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

const PACK_PERSONA: &str = r#"{
  "name": "Pack Persona",
  "description": "from a pack",
  "tools": ["read_file"],
  "system_prompt": "you are a pack persona"
}"#;

#[test]
fn persona_resolves_from_pack_only_when_enabled() {
    // Isolated data dir for this process.
    let data_dir = std::env::temp_dir().join(format!("mc-pack-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    let local_personas = data_dir.join("personas");
    fs::create_dir_all(&local_personas).unwrap();

    // A pack that ships `pack-persona`, laid out like a project.
    let pack_root = data_dir.join("integration_packs").join("testpack");
    write(
        &pack_root.join("pack.json"),
        r#"{"id":"testpack","name":"Test Pack","description":"t","version":"1.0.0"}"#,
    );
    write(&pack_root.join("personas").join("pack-persona.json"), PACK_PERSONA);

    let state_file = data_dir.join("integration_packs.json");

    // 1. Pack disabled (default): resolution fails with an actionable message
    //    naming the pack — not a bare "not found at <path>".
    let err = Persona::load("pack-persona", &local_personas).unwrap_err();
    assert!(
        err.contains("testpack") && err.to_lowercase().contains("disabled"),
        "disabled-pack error should name the pack and say it's disabled, got: {err}"
    );

    // 2. Enable the pack — the persona now resolves from the pack dir.
    write(&state_file, r#"{"testpack":{"enabled":true}}"#);
    let persona = Persona::load("pack-persona", &local_personas)
        .expect("persona should resolve from an enabled pack");
    assert_eq!(persona.name, "Pack Persona");

    // 3. A genuinely missing persona reports the plain not-found message.
    let err = Persona::load("nope", &local_personas).unwrap_err();
    assert!(err.contains("not found"), "got: {err}");

    let _ = fs::remove_dir_all(&data_dir);
}
