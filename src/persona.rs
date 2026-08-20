use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Persona {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    /// Integrations this persona is scoped to, by id (e.g. `"linear"`). Every
    /// HTTP-API tool an installed integration listed here provides is added to
    /// the persona's tool set, so a persona can adopt a whole integration without
    /// enumerating each `<id>_*` tool by name. Combine with `tools` for native
    /// tools like `load_skill`. See [`Persona::resolved_tool_names`].
    ///
    /// Reads `packs` too: that was the field's name until integrations stopped
    /// being installable, and personas carrying it are already on people's pods.
    #[serde(default, alias = "packs")]
    pub integrations: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    /// Optional semantic version (e.g. "1.1.0") for built-in/seed personas.
    /// Drives force-upgrade on startup: when a bundled seed persona's version
    /// is higher than the installed copy's (a missing version counts as 0),
    /// the installed file is overwritten. User-created personas omit this and
    /// are never force-upgraded. See `seed::write_versioned_seeds`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub system_prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PersonaSummary {
    pub slug: String,
    pub name: String,
    pub description: String,
    /// Set when this persona is provided by an enabled integration.
    /// Local (user-owned) personas omit this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_id: Option<String>,
    /// True for pack-provided personas — the workshop disables Save/Delete.
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub read_only: bool,
}

/// Live values spliced into a persona's system prompt that are not part of the
/// persona definition.
///
/// A struct rather than more positional parameters so adding the next dynamic
/// block (per ADR-0001, which requires new dynamic lists to become placeholders
/// rather than hardcoded text) does not churn every call site again.
#[derive(Debug, Clone, Default)]
pub struct PromptExtras {
    /// Rendered memory profile, or empty when memory is off or has nothing
    /// durable to say yet.
    pub memory_profile: String,
}

impl PromptExtras {
    /// Build the extras for a real turn, reading the live memory profile.
    pub async fn load() -> Self {
        Self { memory_profile: crate::memory::profile_block().await }
    }
}

impl Persona {
    /// Load a persona by slug, resolving the user-local `personas/` dir first
    /// and falling back to any enabled integration. Pack personas (e.g.
    /// the discord bundle) load here exactly like local ones.
    pub fn load(slug: &str, personas_dir: &Path) -> Result<Self, String> {
        let (file, _origin) = crate::integrations::resolve_or_explain(
            personas_dir,
            "personas",
            &format!("{}.json", slug),
            "Persona",
            slug,
        )?;

        let content = std::fs::read_to_string(&file)
            .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;

        let persona: Persona = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse {}: {}", file.display(), e))?;

        Ok(persona)
    }

    /// List available persona slugs from the local dir and every enabled pack
    /// (user-local shadows pack on slug collision).
    pub fn list_available(personas_dir: &Path) -> Vec<String> {
        let mut slugs: Vec<String> =
            crate::integrations::list_files_layered(personas_dir, "personas", "json")
                .into_iter()
                .filter_map(|(path, _origin)| {
                    path.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string())
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

    /// List persona summaries (slug + name + description) from the local dir
    /// and every enabled pack, tagging each with its origin so the workshop can
    /// mark pack-provided personas read-only.
    pub fn list_summaries(personas_dir: &Path) -> Vec<PersonaSummary> {
        crate::integrations::list_files_layered(personas_dir, "personas", "json")
            .into_iter()
            .filter_map(|(path, origin)| {
                let slug = path.file_stem().and_then(|s| s.to_str())?.to_string();
                let content = std::fs::read_to_string(&path).ok()?;
                let p: Persona = serde_json::from_str(&content).ok()?;
                Some(PersonaSummary {
                    slug,
                    name: p.name,
                    description: p.description,
                    pack_id: origin.pack_id().map(|s| s.to_string()),
                    read_only: origin.is_read_only(),
                })
            })
            .collect()
    }

    /// The persona's full tool list: the explicitly named `tools` plus every
    /// HTTP-API tool provided by the enabled packs it declares in `packs`
    /// (deduplicated, explicit tools first). This is what the registry and the
    /// step guard should be built from — not the raw `tools` field. A persona
    /// with no `integrations` resolves to exactly its `tools`.
    pub fn resolved_tool_names(&self) -> Vec<String> {
        let mut names = self.tools.clone();
        for id in &self.integrations {
            for tool in crate::tools::http_api::HttpApiTool::installed_tool_names_for_integration(id) {
                if !names.contains(&tool) {
                    names.push(tool);
                }
            }
            // An integration whose tools are native Rust (e.g. s3, which needs SigV4
            // signing) ships no `api_tools/` files, so pull its tool names from the
            // native registry too — but only when it is actually installed, matching
            // the HTTP-API path above. Without that gate an uninstalled native-tool
            // integration keeps leaking its tools into any persona that pins it.
            if crate::integrations::is_enabled(id) {
                for tool in crate::tools::native_integration_tool_names(id) {
                    if !names.contains(&tool) {
                        names.push(tool);
                    }
                }
            }
        }
        names
    }

    /// Build the system prompt with no dynamic extras. Equivalent to
    /// [`Self::build_system_prompt_with`] with [`PromptExtras::default`], and the
    /// right call for diagnostics or anywhere the live memory profile is not
    /// wanted.
    pub fn build_system_prompt(&self, skills_dir: &Path, cwd: &str) -> String {
        self.build_system_prompt_with(skills_dir, cwd, &PromptExtras::default())
    }

    /// Build the system prompt. The persona's `system_prompt` is treated as a
    /// template: `{{cwd}}`, `{{available_skills}}`, `{{available_personas}}`,
    /// `{{installed_packs}}`, `{{now_utc}}`, and `{{memory_profile}}` are
    /// substituted with live values
    /// so an author can place each exactly where they want it. Any of those the
    /// template does NOT reference is appended afterward with a default heading,
    /// preserving the behavior of personas written before templating.
    pub fn build_system_prompt_with(
        &self,
        skills_dir: &Path,
        cwd: &str,
        extras: &PromptExtras,
    ) -> String {
        let skills_block = self.skills_block(skills_dir);
        let personas_block = self.personas_block();
        let packs_block = installed_packs_block();

        // Ground the model in the current instant. Without this it has no
        // reliable "today", so it can't resolve relative dates ("tomorrow") —
        // the root of the calendar-in-UTC bug. Given in UTC; downstream tools
        // (e.g. the calendar's per-calendar timezone) localize from there.
        let now = chrono::Utc::now();
        let now_utc = now.format("%Y-%m-%dT%H:%M:%SZ (%A)").to_string();

        let vars = [
            ("cwd", cwd.to_string()),
            ("available_skills", skills_block.clone()),
            ("available_personas", personas_block.clone()),
            ("installed_packs", packs_block),
            ("now_utc", now_utc.clone()),
            ("memory_profile", extras.memory_profile.clone()),
        ];

        let mut prompt = render_template(&self.system_prompt, &vars);

        // Fallback append: only for lists the template didn't already place,
        // so authored placeholders never produce a duplicate section.
        if !template_uses(&self.system_prompt, "cwd") {
            prompt.push_str(&format!("\n\nWorking directory: {}", cwd));
        }

        // Always surface "now" (it's never empty); skip only if the author
        // placed {{now_utc}} themselves.
        if !template_uses(&self.system_prompt, "now_utc") {
            prompt.push_str(&format!("\n\nCurrent time: {} (UTC).", now_utc));
        }

        // The operator profile: stable, slow-moving memory (pinned facts,
        // preferences, working methods). Deliberately NOT the per-turn recall —
        // that goes in the message tail, because changing the system prompt every
        // turn would defeat the provider's prompt cache.
        if !extras.memory_profile.is_empty()
            && !template_uses(&self.system_prompt, "memory_profile")
        {
            prompt.push_str("\n\n# What You Remember About This User\n");
            prompt.push_str(&extras.memory_profile);
        }

        if !skills_block.is_empty() && !template_uses(&self.system_prompt, "available_skills") {
            prompt.push_str("\n\n# Available Skills\n");
            prompt.push_str("You have access to the `load_skill` tool. Call it with a skill name to load detailed guidance.\n");
            prompt.push_str("Available skills:\n");
            prompt.push_str(&skills_block);
        }

        // Personas that can delegate (the `sub_agent` tool) need to know which
        // specialist personas are actually installed/enabled — otherwise they
        // can only guess slugs from the hardcoded examples in their prompt and
        // will silently skip delegating to packs added after the prompt was
        // written (e.g. cloudflare-agent). Inject the live list so the model
        // picks a real `persona` value instead of inferring one.
        if self.tools.iter().any(|t| t == "sub_agent")
            && !personas_block.is_empty()
            && !template_uses(&self.system_prompt, "available_personas")
        {
            prompt.push_str("\n\n# Available Sub-Agent Personas\n");
            prompt.push_str("Pass one of these slugs as `persona` to `sub_agent` to delegate to a specialist. This is the live list of installed/enabled personas — prefer it over any examples above.\n");
            prompt.push_str(&personas_block);
        }

        prompt
    }

    /// Bulleted list of this persona's skills with descriptions (one per line,
    /// trailing newline). Empty when the persona declares no skills.
    fn skills_block(&self, skills_dir: &Path) -> String {
        let mut out = String::new();
        for skill in &self.skills {
            let desc = load_skill_description(skill, skills_dir);
            out.push_str(&format!("- **{}**: {}\n", skill, desc));
        }
        out
    }

    /// Bulleted list of installed/enabled specialist personas (excluding self)
    /// as `slug (pack: id): description`. Empty when none besides self exist.
    fn personas_block(&self) -> String {
        let mut out = String::new();
        for s in Self::list_summaries(&crate::paths::personas_dir()) {
            if s.name == self.name {
                continue;
            }
            let pack = s
                .pack_id
                .as_deref()
                .map(|p| format!(" (pack: {p})"))
                .unwrap_or_default();
            out.push_str(&format!("- **{}**{}: {}\n", s.slug, pack, s.description));
        }
        out
    }

}

/// Bulleted list of enabled integrations as `id (name): description`
/// (one per line, trailing newline). Empty when no packs are enabled.
fn installed_packs_block() -> String {
    let mut out = String::new();
    for pack in crate::integrations::installed_integrations() {
        let m = &pack.manifest;
        out.push_str(&format!("- **{}** ({}): {}\n", m.id, m.name, m.description));
    }
    out
}

/// True if `template` references the `{{name}}` variable (optionally padded
/// with inner whitespace, e.g. `{{ name }}`).
fn template_uses(template: &str, name: &str) -> bool {
    template.match_indices("{{").any(|(open, _)| {
        template[open + 2..]
            .find("}}")
            .is_some_and(|close| template[open + 2..open + 2 + close].trim() == name)
    })
}

/// Minimal mustache-style renderer: replaces every `{{ name }}` occurrence
/// (inner whitespace ignored) with the matching value. Unknown placeholders
/// are left untouched so a typo is visible rather than silently dropped.
fn render_template(template: &str, vars: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let Some(close_rel) = rest[open + 2..].find("}}") else {
            break;
        };
        let key = rest[open + 2..open + 2 + close_rel].trim();
        out.push_str(&rest[..open]);
        match vars.iter().find(|(k, _)| *k == key) {
            Some((_, val)) => out.push_str(val.trim_end_matches('\n')),
            None => out.push_str(&rest[open..open + 2 + close_rel + 2]),
        }
        rest = &rest[open + 2 + close_rel + 2..];
    }
    out.push_str(rest);
    out
}

/// Parse YAML frontmatter description from a skill file, resolving the local
/// `skills/` dir first and falling back to any enabled integration.
fn load_skill_description(name: &str, skills_dir: &Path) -> String {
    let Some((file, _origin)) =
        crate::integrations::resolve_file(skills_dir, "skills", &format!("{}.md", name))
    else {
        return "Specialized guidance".to_string();
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_known_vars_and_ignores_whitespace() {
        let out = render_template(
            "dir={{cwd}} packs:\n{{ installed_packs }}done",
            &[
                ("cwd", "/tmp".to_string()),
                ("installed_packs", "- a\n- b\n".to_string()),
            ],
        );
        // trailing newline of a list value is trimmed at the splice point
        assert_eq!(out, "dir=/tmp packs:\n- a\n- bdone");
    }

    #[test]
    fn leaves_unknown_placeholder_untouched() {
        let out = render_template("x={{nope}}", &[("cwd", "/tmp".to_string())]);
        assert_eq!(out, "x={{nope}}");
    }

    #[test]
    fn template_uses_detects_padded_var() {
        assert!(template_uses("a {{ available_personas }} b", "available_personas"));
        assert!(template_uses("{{cwd}}", "cwd"));
        assert!(!template_uses("{{cwdx}}", "cwd"));
        assert!(!template_uses("no placeholders here", "cwd"));
    }
}
