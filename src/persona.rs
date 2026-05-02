use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Persona {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    pub system_prompt: String,
}

impl Persona {
    /// Load a persona by slug name from the personas directory.
    /// Looks for `<personas_dir>/<slug>.json`.
    pub fn load(slug: &str, personas_dir: &Path) -> Result<Self, String> {
        let file = personas_dir.join(format!("{}.json", slug));
        if !file.exists() {
            return Err(format!("Persona '{}' not found at {}", slug, file.display()));
        }

        let content = std::fs::read_to_string(&file)
            .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;

        let persona: Persona = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse {}: {}", file.display(), e))?;

        Ok(persona)
    }

    /// List available persona slugs from the personas directory.
    pub fn list_available(personas_dir: &Path) -> Vec<String> {
        let entries = match std::fs::read_dir(personas_dir) {
            Ok(rd) => rd,
            Err(_) => return vec![],
        };

        let mut slugs: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) == Some("json") {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect();

        slugs.sort();
        slugs
    }

    /// Build the system prompt. Lists available skills by name — use `load_skill` tool to load on demand.
    pub fn build_system_prompt(&self, _skills_dir: &Path, cwd: &str) -> String {
        let mut prompt = self.system_prompt.clone();

        prompt.push_str(&format!("\n\nWorking directory: {}", cwd));

        if !self.skills.is_empty() {
            prompt.push_str("\n\n# Available Skills\n");
            prompt.push_str("You have access to the `load_skill` tool. Call it with a skill name to load detailed guidance.\n");
            prompt.push_str("Available skills:\n");
            for skill in &self.skills {
                let desc = load_skill_description(skill, _skills_dir);
                prompt.push_str(&format!("- **{}**: {}\n", skill, desc));
            }
        }

        prompt
    }

    /// Resolve the default personas directory relative to the executable or repo.
    pub fn default_personas_dir() -> PathBuf {
        // Check relative to cwd first (repo layout)
        let cwd_based = PathBuf::from("personas");
        if cwd_based.is_dir() {
            return cwd_based;
        }

        // Fallback: next to executable
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                let exe_based = parent.join("personas");
                if exe_based.is_dir() {
                    return exe_based;
                }
            }
        }

        cwd_based
    }

    /// Resolve the default skills directory.
    pub fn default_skills_dir() -> PathBuf {
        let cwd_based = PathBuf::from("skills");
        if cwd_based.is_dir() {
            return cwd_based;
        }

        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                let exe_based = parent.join("skills");
                if exe_based.is_dir() {
                    return exe_based;
                }
            }
        }

        cwd_based
    }
}

/// Parse YAML frontmatter description from a skill file.
fn load_skill_description(name: &str, skills_dir: &Path) -> String {
    let file = skills_dir.join(format!("{}.md", name));
    let content = match std::fs::read_to_string(&file) {
        Ok(c) => c,
        Err(_) => return "Specialized guidance".to_string(),
    };
    parse_frontmatter_description(&content)
        .unwrap_or_else(|| "Specialized guidance".to_string())
}

/// Extract `description` from YAML frontmatter (between `---` delimiters).
pub fn parse_frontmatter_description(content: &str) -> Option<String> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }
    let after_open = &content[3..];
    let close_pos = after_open.find("\n---")?;
    let yaml_block = &after_open[..close_pos];
    for line in yaml_block.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("description:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Strip YAML frontmatter from skill content, returning just the body.
pub fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return content;
    }
    let after_open = &trimmed[3..];
    match after_open.find("\n---") {
        Some(pos) => {
            let after_close = &after_open[pos + 4..];
            after_close.trim_start_matches('\n')
        }
        None => content,
    }
}
