//! Regression test for the bug where flow runs failed with
//! "Persona '<slug>' not found" because the persona lived in an integration
//! pack rather than the local `personas/` dir. Runtime persona loading must
//! resolve packs as a fallback.
//!
//! The enable/disable half of this test is gone on purpose. Availability is no
//! longer a mutable flag: a tool resolves when an installed pack provides it, the
//! persona references it, and the preset declares it. An installed pack is
//! available, full stop — so there is no "installed but disabled" state left to
//! report, and this now checks that an *installed* pack resolves and a missing one
//! reports a plain not-found.
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
fn persona_resolves_from_an_installed_pack() {
    // Isolated data dir for this process.
    let data_dir = std::env::temp_dir().join(format!("mc-pack-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data_dir);
    unsafe {
        std::env::set_var("METALCRAFT_DATA_DIR", &data_dir);
    }

    let local_personas = data_dir.join("personas");
    fs::create_dir_all(&local_personas).unwrap();

    // A pack that ships `pack-persona`, laid out like a project.
    let pack_root = data_dir.join("integrations").join("testpack");
    write(
        &pack_root.join("integration.json"),
        r#"{"id":"testpack","name":"Test Pack","description":"t","version":"1.0.0"}"#,
    );
    write(&pack_root.join("personas").join("pack-persona.json"), PACK_PERSONA);

    // 1. The pack is installed, so its persona resolves — no flag to flip first.
    let persona = Persona::load("pack-persona", &local_personas)
        .expect("a persona in an installed pack must resolve");
    assert_eq!(persona.name, "Pack Persona");

    // 2. A stale `enabled: false` from before the flag was retired must not hide it.
    //    Upgraded pods carry these, and honouring them would resurrect the bug.
    write(&data_dir.join("integrations.json"), r#"{"testpack":{"enabled":false}}"#);
    let persona = Persona::load("pack-persona", &local_personas)
        .expect("a stale disabled flag must not affect resolution");
    assert_eq!(persona.name, "Pack Persona");

    // 3. A genuinely missing persona reports the plain not-found message.
    let err = Persona::load("nope", &local_personas).unwrap_err();
    assert!(err.contains("not found"), "got: {err}");

    let _ = fs::remove_dir_all(&data_dir);
}
