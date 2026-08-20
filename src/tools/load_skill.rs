use async_trait::async_trait;
use crate::persona::strip_frontmatter;
use std::path::PathBuf;

pub struct LoadSkillTool {
    skills_dir: PathBuf,
    available_skills: Vec<String>,
}

impl LoadSkillTool {
    pub fn new(skills_dir: PathBuf, available_skills: Vec<String>) -> Self {
        Self { skills_dir, available_skills }
    }
}

#[async_trait]
impl metalcraft::Tool for LoadSkillTool {
    fn name(&self) -> &str { "load_skill" }
    fn description(&self) -> &str {
        "Load a skill by name to get detailed guidance for a specific task type. Use this when you need structured methodology for a task (e.g. debugging, code-review, planning)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The skill name to load",
                    "enum": self.available_skills
                }
            },
            "required": ["skill"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let skill_name = args["skill"].as_str().ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
            tool: "load_skill".into(),
            message: "Missing required parameter: skill".into(),
        })?;

        if !self.available_skills.contains(&skill_name.to_string()) {
            return Ok(serde_json::json!({
                "error": format!("Unknown skill '{}'. Available: {:?}", skill_name, self.available_skills)
            }));
        }

        // Resolve the local skills dir first, then any enabled integration
        // pack (e.g. discord ships its own skills).
        let (file, _origin) = crate::integrations::resolve_file(
            &self.skills_dir,
            "skills",
            &format!("{}.md", skill_name),
        )
        .ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
            tool: "load_skill".into(),
            message: format!("Skill '{}' not found in skills dir or any enabled pack", skill_name),
        })?;
        let content = std::fs::read_to_string(&file).map_err(|e| metalcraft::GraphError::ToolCallFailed {
            tool: "load_skill".into(),
            message: format!("Failed to read skill '{}': {}", skill_name, e),
        })?;

        let body = strip_frontmatter(&content);

        Ok(serde_json::json!({
            "skill": skill_name,
            "content": body
        }))
    }
}
