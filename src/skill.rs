//! Skill files (`skills/<slug>.md`) and their CRUD, shared by the workshop
//! HTTP API and the `skill_*` meta tools so the GUI and the prompt-driven path
//! read/write skills identically. A skill is a markdown doc with a YAML
//! frontmatter `description:` and a body; `load_skill` strips the frontmatter
//! when serving the body to a model.

use serde::{Deserialize, Serialize};

use crate::paths;

/// Summary of a skill for listings (no body).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct SkillSummary {
    pub slug: String,
    pub description: String,
    /// Set when this skill is provided by an enabled integration pack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_id: Option<String>,
    /// True for pack-provided skills — the workshop disables Save/Delete.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub read_only: bool,
}

/// A full skill (frontmatter description + body).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Skill {
    pub slug: String,
    pub description: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_id: Option<String>,
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub read_only: bool,
}

/// List every skill (local dir + enabled packs), sorted by slug. A local skill
/// shadows a pack skill of the same slug.
pub fn list_skill_summaries() -> Vec<SkillSummary> {
    let layered =
        crate::integration_packs::list_files_layered(&paths::skills_dir(), "skills", "md");
    let mut summaries: Vec<SkillSummary> = layered
        .into_iter()
        .filter_map(|(path, origin)| {
            let slug = path.file_stem()?.to_str()?.to_string();
            let content = std::fs::read_to_string(&path).ok()?;
            let description = crate::persona::parse_frontmatter_description(&content)
                .unwrap_or_else(|| "No description".to_string());
            Some(SkillSummary {
                slug,
                description,
                pack_id: origin.pack_id().map(String::from),
                read_only: origin.is_read_only(),
            })
        })
        .collect();
    summaries.sort_by(|a, b| a.slug.cmp(&b.slug));
    summaries
}

/// Load one skill by slug, resolving the local dir first then enabled packs.
pub fn load_skill(slug: &str) -> Option<Skill> {
    let filename = format!("{slug}.md");
    let (path, origin) =
        crate::integration_packs::resolve_file(&paths::skills_dir(), "skills", &filename)?;
    let content = std::fs::read_to_string(&path).ok()?;
    let description = crate::persona::parse_frontmatter_description(&content).unwrap_or_default();
    let body = crate::persona::strip_frontmatter(&content).to_string();
    Some(Skill {
        slug: slug.to_string(),
        description,
        body,
        pack_id: origin.pack_id().map(String::from),
        read_only: origin.is_read_only(),
    })
}

/// Write a skill to the local dir, reassembling the frontmatter + body.
pub fn save_skill(slug: &str, skill: &Skill) -> Result<(), String> {
    let path = paths::skills_dir().join(format!("{slug}.md"));
    let content = format!(
        "---\ndescription: {}\n---\n\n{}",
        skill.description, skill.body
    );
    std::fs::write(&path, content).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

/// True when `slug` is currently provided by an integration pack and there is
/// no local file shadowing it — i.e. writing/deleting it via the user path must
/// be refused. Returns the owning pack id for the error message.
pub fn pack_owner_blocking_write(slug: &str) -> Option<String> {
    let filename = format!("{slug}.md");
    if paths::skills_dir().join(&filename).exists() {
        return None;
    }
    crate::integration_packs::resolve_file(&paths::skills_dir(), "skills", &filename)
        .and_then(|(_, origin)| origin.pack_id().map(String::from))
}

/// Delete a local skill file by slug. Errors if no local file exists.
pub fn delete_skill(slug: &str) -> Result<(), String> {
    let path = paths::skills_dir().join(format!("{slug}.md"));
    if !path.exists() {
        return Err(format!("skill '{slug}' is not a user-local skill"));
    }
    std::fs::remove_file(&path).map_err(|e| format!("Failed to delete: {e}"))
}
