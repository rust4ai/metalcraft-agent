use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaSummary {
    pub slug: String,
    pub name: String,
    pub description: String,
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

    /// Save this persona to disk under the given slug.
    pub fn save(&self, slug: &str, personas_dir: &Path) -> Result<(), String> {
        let file = personas_dir.join(format!("{}.json", slug));
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize persona: {e}"))?;
        std::fs::write(&file, content)
            .map_err(|e| format!("Failed to write {}: {e}", file.display()))
    }

    /// Delete a persona by slug.
    pub fn delete(slug: &str, personas_dir: &Path) -> Result<(), String> {
        let file = personas_dir.join(format!("{}.json", slug));
        if !file.exists() {
            return Err(format!("Persona '{}' not found", slug));
        }
        std::fs::remove_file(&file)
            .map_err(|e| format!("Failed to delete {}: {e}", file.display()))
    }

    /// List persona summaries (slug + name + description) from the personas directory.
    pub fn list_summaries(personas_dir: &Path) -> Vec<PersonaSummary> {
        Self::list_available(personas_dir)
            .into_iter()
            .filter_map(|slug| {
                Self::load(&slug, personas_dir).ok().map(|p| PersonaSummary {
                    slug,
                    name: p.name,
                    description: p.description,
                })
            })
            .collect()
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
