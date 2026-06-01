use metalcraft_agent::persona::Persona;
use std::path::Path;

#[test]
fn load_coding_agent_persona() {
    let personas_dir = Path::new("personas");
    let persona = Persona::load("coding-agent", personas_dir).unwrap();

    assert_eq!(persona.name, "Coding Agent");
    assert!(persona.tools.contains(&"read_file".to_string()));
    assert!(persona.tools.contains(&"bash".to_string()));
    assert!(persona.skills.contains(&"commit-message".to_string()));
    assert!(!persona.system_prompt.is_empty());
}

#[test]
fn load_research_agent_persona() {
    let personas_dir = Path::new("personas");
    let persona = Persona::load("research-agent", personas_dir).unwrap();

    assert_eq!(persona.name, "Research Agent");
    assert!(persona.tools.contains(&"read_file".to_string()));
    assert!(persona.tools.contains(&"grep".to_string()));
    // Research agent should NOT have write_file
    assert!(!persona.tools.contains(&"write_file".to_string()));
}

#[test]
fn load_devops_agent_persona() {
    let personas_dir = Path::new("personas");
    let persona = Persona::load("devops-agent", personas_dir).unwrap();

    assert_eq!(persona.name, "DevOps Agent");
    assert!(persona.tools.contains(&"bash".to_string()));
    assert!(persona.skills.contains(&"dockerfile-best-practices".to_string()));
}

#[test]
fn list_available_personas() {
    let personas_dir = Path::new("personas");
    let available = Persona::list_available(personas_dir);

    assert!(available.contains(&"coding-agent".to_string()));
    assert!(available.contains(&"research-agent".to_string()));
    assert!(available.contains(&"devops-agent".to_string()));
}

#[test]
fn load_nonexistent_persona_fails() {
    let personas_dir = Path::new("personas");
    let result = Persona::load("nonexistent", personas_dir);
    assert!(result.is_err());
}

#[test]
fn build_system_prompt_includes_skills() {
    let personas_dir = Path::new("personas");
    let skills_dir = Path::new("skills");
    let persona = Persona::load("coding-agent", personas_dir).unwrap();

    let prompt = persona.build_system_prompt(skills_dir, "/tmp/test");

    assert!(prompt.contains("Working directory: /tmp/test"));
    assert!(prompt.contains("# Available Skills"));
    assert!(prompt.contains("load_skill"));
    assert!(prompt.contains("commit-message")); // skill name listed
    assert!(prompt.contains("code-review"));    // skill name listed
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
