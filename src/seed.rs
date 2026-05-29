use crate::paths;
use std::fs;
use std::path::Path;

const SEED_PERSONAS: &[(&str, &str)] = &[
    ("coding-agent.json", include_str!("../seed/personas/coding-agent.json")),
    ("devops-agent.json", include_str!("../seed/personas/devops-agent.json")),
    ("discord-agent.json", include_str!("../seed/personas/discord-agent.json")),
    ("discord-reporter-agent.json", include_str!("../seed/personas/discord-reporter-agent.json")),
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

const SEED_API_TOOLS: &[(&str, &str)] = &[
    ("discord_send_message.json", include_str!("../seed/api_tools/discord_send_message.json")),
    ("discord_edit_message.json", include_str!("../seed/api_tools/discord_edit_message.json")),
    ("discord_add_reaction.json", include_str!("../seed/api_tools/discord_add_reaction.json")),
    ("discord_get_messages.json", include_str!("../seed/api_tools/discord_get_messages.json")),
    ("discord_get_channel_info.json", include_str!("../seed/api_tools/discord_get_channel_info.json")),
];

/// Flows live in the user's project — we no longer seed any by default so
/// the workshop's "+ New Flow" picker (template vs blank) is the canonical
/// entry point.
const SEED_FLOWS: &[(&str, &str)] = &[];

/// Templates the workshop offers via "+ New Flow → from template". Copied
/// into `<data>/flow_templates/` on first run so they're editable per-deploy.
const SEED_FLOW_TEMPLATES: &[(&str, &str)] = &[
    ("daily-commit-summary.json", include_str!("../seed/flow_templates/daily-commit-summary.json")),
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
        paths::flow_templates_dir(),
    ];

    for dir in &dirs {
        if let Err(e) = fs::create_dir_all(dir) {
            eprintln!("Warning: could not create {}: {e}", dir.display());
        }
    }

    write_seeds(&paths::personas_dir(), SEED_PERSONAS);
    write_seeds(&paths::skills_dir(), SEED_SKILLS);
    write_seeds(&paths::flows_dir(), SEED_FLOWS);
    write_seeds(&paths::api_tools_dir(), SEED_API_TOOLS);
    write_seeds(&paths::flow_templates_dir(), SEED_FLOW_TEMPLATES);
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
