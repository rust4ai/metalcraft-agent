use crate::paths;
use std::fs;
use std::path::Path;

const SEED_PERSONAS: &[(&str, &str)] = &[
    ("coding-agent.json", include_str!("../seed/personas/coding-agent.json")),
    ("devops-agent.json", include_str!("../seed/personas/devops-agent.json")),
    ("discord-agent.json", include_str!("../seed/personas/discord-agent.json")),
    ("research-agent.json", include_str!("../seed/personas/research-agent.json")),
    ("video-script-agent.json", include_str!("../seed/personas/video-script-agent.json")),
];

const SEED_SKILLS: &[(&str, &str)] = &[
    ("ci-cd.md", include_str!("../seed/skills/ci-cd.md")),
    ("code-review.md", include_str!("../seed/skills/code-review.md")),
    ("commit-message.md", include_str!("../seed/skills/commit-message.md")),
    ("debugging.md", include_str!("../seed/skills/debugging.md")),
    ("dockerfile-best-practices.md", include_str!("../seed/skills/dockerfile-best-practices.md")),
    ("edit-workflow.md", include_str!("../seed/skills/edit-workflow.md")),
    ("explore-codebase.md", include_str!("../seed/skills/explore-codebase.md")),
    ("planning.md", include_str!("../seed/skills/planning.md")),
    ("research-methodology.md", include_str!("../seed/skills/research-methodology.md")),
    ("summarize.md", include_str!("../seed/skills/summarize.md")),
    ("video-scripting.md", include_str!("../seed/skills/video-scripting.md")),
    ("discord-etiquette.md", include_str!("../seed/skills/discord-etiquette.md")),
    ("discord-formatting.md", include_str!("../seed/skills/discord-formatting.md")),
];

/// Ensure default personas and skills exist in the app data directory.
/// Creates directories and writes seed files only if they don't already exist.
pub fn ensure_defaults() {
    let dirs = [
        paths::personas_dir(),
        paths::skills_dir(),
        paths::flows_dir(),
        paths::logs_dir(),
        paths::api_tools_dir(),
    ];

    for dir in &dirs {
        if let Err(e) = fs::create_dir_all(dir) {
            eprintln!("Warning: could not create {}: {e}", dir.display());
        }
    }

    write_seeds(&paths::personas_dir(), SEED_PERSONAS);
    write_seeds(&paths::skills_dir(), SEED_SKILLS);
}

fn write_seeds(dir: &Path, seeds: &[(&str, &str)]) {
    for (filename, content) in seeds {
        let target = dir.join(filename);
        if !target.exists() {
            if let Err(e) = fs::write(&target, content) {
                eprintln!("Warning: could not write {}: {e}", target.display());
            }
        }
    }
}
