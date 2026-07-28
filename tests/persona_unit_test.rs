use metalcraft_agent::persona::Persona;
use std::path::Path;

// NOTE: the persona-loading tests that read a top-level `personas/` directory
// were removed — personas now live under `seed/personas/` and are seeded into
// the runtime data dir via integration packs, so that fixture no longer exists.
// Loading personas by slug is covered end-to-end by the `*_spice_test.rs`
// harnesses (which seed a temp data dir); the tests below cover the parts of
// `persona` that don't depend on an on-disk personas directory.

#[test]
fn load_nonexistent_persona_fails() {
    let personas_dir = Path::new("personas");
    let result = Persona::load("nonexistent", personas_dir);
    assert!(result.is_err());
}

#[test]
fn build_system_prompt_without_skills() {
    let persona = Persona {
        name: "Test".into(),
        description: "Test persona".into(),
        tools: vec![],
        packs: vec![],
        skills: vec![],
        version: None,
        system_prompt: "You are a test.".into(),
    };

    let prompt = persona.build_system_prompt(Path::new("skills"), "/tmp");
    assert!(prompt.contains("You are a test."));
    assert!(prompt.contains("Working directory: /tmp"));
    assert!(!prompt.contains("# Skills"));
}

#[test]
fn create_registry_for_subset() {
    let tools = vec!["read_file".to_string(), "grep".to_string()];
    let registry = metalcraft_agent::tools::create_registry_for(&tools);
    let names = registry.names();
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"grep"));
    assert!(!names.contains(&"bash"));
    assert!(!names.contains(&"write_file"));
}
