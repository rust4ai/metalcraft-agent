//! Meta tools for authoring **skills** by prompt — the workshop's skill CRUD,
//! delegating to `crate::skill` so the GUI and the prompt path write identical
//! files. A skill is a markdown doc with a frontmatter `description:` and a
//! body. Writes/deletes refuse pack-provided slugs.

use async_trait::async_trait;

use crate::skill::{self, Skill};
use crate::tools::missing_param;

fn slug_arg(args: &serde_json::Value, tool: &str) -> metalcraft::Result<String> {
    args["slug"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| missing_param(tool, "slug"))
}

pub struct SkillListTool;

#[async_trait]
impl metalcraft::Tool for SkillListTool {
    fn name(&self) -> &str {
        "skill_list"
    }
    fn description(&self) -> &str {
        "List all skills (local + enabled packs) with slug, description, and whether each is pack-provided (read-only)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }
    async fn call(&self, _args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        Ok(serde_json::json!({ "skills": skill::list_skill_summaries() }))
    }
}

pub struct SkillReadTool;

#[async_trait]
impl metalcraft::Tool for SkillReadTool {
    fn name(&self) -> &str {
        "skill_read"
    }
    fn description(&self) -> &str {
        "Read one skill by slug, returning its description and full markdown body."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "slug": { "type": "string", "description": "Skill slug (filename without .md)" } },
            "required": ["slug"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let slug = slug_arg(&args, "skill_read")?;
        match skill::load_skill(&slug) {
            Some(s) => Ok(serde_json::to_value(s).unwrap_or(serde_json::Value::Null)),
            None => Ok(serde_json::json!({ "error": format!("skill '{slug}' not found") })),
        }
    }
}

pub struct SkillWriteTool;

#[async_trait]
impl metalcraft::Tool for SkillWriteTool {
    fn name(&self) -> &str {
        "skill_write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a skill. Provide `slug`, a one-line `description` (becomes the YAML frontmatter), and the markdown `body`. Refuses slugs owned by an integration pack."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "slug": { "type": "string", "description": "Skill slug to write (filename without .md)" },
                "description": { "type": "string", "description": "One-line description (YAML frontmatter)" },
                "body": { "type": "string", "description": "Markdown body of the skill" }
            },
            "required": ["slug", "description", "body"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let slug = slug_arg(&args, "skill_write")?;
        let description = args["description"]
            .as_str()
            .ok_or_else(|| missing_param("skill_write", "description"))?;
        let body = args["body"]
            .as_str()
            .ok_or_else(|| missing_param("skill_write", "body"))?;

        if let Some(pack_id) = skill::pack_owner_blocking_write(&slug) {
            return Ok(serde_json::json!({
                "error": format!("skill '{slug}' is provided by the '{pack_id}' integration pack and is read-only. Choose a different slug.")
            }));
        }

        let skill = Skill {
            slug: slug.clone(),
            description: description.to_string(),
            body: body.to_string(),
            pack_id: None,
            read_only: false,
        };
        match skill::save_skill(&slug, &skill) {
            Ok(()) => Ok(serde_json::json!({ "saved": slug })),
            Err(e) => Ok(serde_json::json!({ "error": e })),
        }
    }
}

pub struct SkillDeleteTool;

#[async_trait]
impl metalcraft::Tool for SkillDeleteTool {
    fn name(&self) -> &str {
        "skill_delete"
    }
    fn description(&self) -> &str {
        "Delete a user-local skill by slug. Pack-provided skills cannot be deleted."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "slug": { "type": "string", "description": "Skill slug to delete" } },
            "required": ["slug"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let slug = slug_arg(&args, "skill_delete")?;
        match skill::delete_skill(&slug) {
            Ok(()) => Ok(serde_json::json!({ "deleted": slug })),
            Err(e) => Ok(serde_json::json!({ "error": e })),
        }
    }
}
