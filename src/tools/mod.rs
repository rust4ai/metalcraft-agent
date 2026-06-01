pub mod bash;
pub mod edit_file;
pub mod find_files;
pub mod grep;
pub mod http_api;
pub mod list_files;
pub mod load_skill;
pub mod meta_diagnostics;
pub mod meta_flow;
pub mod meta_persona;
pub mod meta_skill;
pub mod read_file;
pub mod spaces;
pub mod sub_agent;
pub mod web_fetch;
pub mod write_file;

use std::path::PathBuf;
use metalcraft::ToolRegistry;

/// Configuration for tools that need runtime parameters.
pub struct ToolConfig {
    pub api_key: String,
    pub model_name: String,
    pub system_prompt: String,
    pub skills_dir: PathBuf,
    pub available_skills: Vec<String>,
}

/// Register only the tools listed by name.
pub fn create_registry_for(tool_names: &[String]) -> ToolRegistry {
    create_registry_for_with_config(tool_names, None)
}

/// Register tools with optional config for tools that need runtime parameters (e.g. sub_agent).
pub fn create_registry_for_with_config(
    tool_names: &[String],
    config: Option<&ToolConfig>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for name in tool_names {
        registry = match name.as_str() {
            "read_file" => registry.register(read_file::ReadFileTool),
            "write_file" => registry.register(write_file::WriteFileTool),
            "edit_file" => registry.register(edit_file::EditFileTool),
            "bash" => registry.register(bash::BashTool),
            "list_files" => registry.register(list_files::ListFilesTool),
            "grep" => registry.register(grep::GrepTool),
            "find_files" => registry.register(find_files::FindFilesTool),
            "load_skill" => {
                if let Some(cfg) = config {
                    registry.register(load_skill::LoadSkillTool::new(
                        cfg.skills_dir.clone(),
                        cfg.available_skills.clone(),
                    ))
                } else {
                    log::warn!("load_skill tool requires ToolConfig, skipping");
                    registry
                }
            }
            "web_fetch" => registry.register(web_fetch::WebFetchTool),
            // Meta tools: author/manage the metalcraft project itself (the
            // workshop's CRUD surface, by prompt). They operate on the global
            // `paths::*` dirs, so they need no ToolConfig.
            "persona_list" => registry.register(meta_persona::PersonaListTool),
            "persona_read" => registry.register(meta_persona::PersonaReadTool),
            "persona_write" => registry.register(meta_persona::PersonaWriteTool),
            "persona_delete" => registry.register(meta_persona::PersonaDeleteTool),
            "skill_list" => registry.register(meta_skill::SkillListTool),
            "skill_read" => registry.register(meta_skill::SkillReadTool),
            "skill_write" => registry.register(meta_skill::SkillWriteTool),
            "skill_delete" => registry.register(meta_skill::SkillDeleteTool),
            "flow_list" => registry.register(meta_flow::FlowListTool),
            "flow_read" => registry.register(meta_flow::FlowReadTool),
            "flow_validate" => registry.register(meta_flow::FlowValidateTool),
            "flow_write" => registry.register(meta_flow::FlowWriteTool),
            "flow_delete" => registry.register(meta_flow::FlowDeleteTool),
            "flow_run" => registry.register(meta_flow::FlowRunTool),
            "flow_templates_list" => registry.register(meta_flow::FlowTemplatesListTool),
            "flow_template_read" => registry.register(meta_flow::FlowTemplateReadTool),
            "diagnostics_list" => registry.register(meta_diagnostics::DiagnosticsListTool),
            "diagnostics_read" => registry.register(meta_diagnostics::DiagnosticsReadTool),
            // DigitalOcean Spaces (S3-compatible) file storage — native tools
            // because S3 requires per-request AWS SigV4 signing the declarative
            // HTTP-API tool can't produce. Shipped by the `digitalocean_spaces`
            // pack; read DO_SPACES_KEY/SECRET/REGION from the key store.
            "spaces_list_buckets" => registry.register(spaces::SpacesListBucketsTool),
            "spaces_list_objects" => registry.register(spaces::SpacesListObjectsTool),
            "spaces_get_object" => registry.register(spaces::SpacesGetObjectTool),
            "spaces_put_object" => registry.register(spaces::SpacesPutObjectTool),
            "spaces_delete_object" => registry.register(spaces::SpacesDeleteObjectTool),
            "sub_agent" => {
                if let Some(cfg) = config {
                    registry.register(sub_agent::SubAgentTool::new(
                        cfg.api_key.clone(),
                        cfg.model_name.clone(),
                        cfg.system_prompt.clone(),
                    ))
                } else {
                    log::warn!("sub_agent tool requires ToolConfig, skipping");
                    registry
                }
            }
            unknown => {
                // Try loading as a user-defined HTTP API tool
                if let Some(api_tool) = http_api::HttpApiTool::try_load(unknown) {
                    registry.register(api_tool)
                } else {
                    log::warn!("Unknown tool '{}' in persona, skipping", unknown);
                    registry
                }
            }
        };
    }
    registry
}

/// Register all available tools.
pub fn create_registry() -> ToolRegistry {
    let all = vec![
        "read_file", "write_file", "edit_file", "bash",
        "list_files", "grep", "find_files",
    ].into_iter().map(String::from).collect::<Vec<_>>();
    create_registry_for(&all)
}

/// Native (Rust) tool names contributed by an integration pack, keyed by pack
/// id. Most packs ship only declarative HTTP-API tools (discovered from the
/// pack directory by [`http_api::HttpApiTool::installed_tool_names_for_pack`]);
/// a few — like `digitalocean_spaces`, whose S3 SigV4 signing the HTTP-API tool
/// can't produce — ship native tools registered by name in
/// [`create_registry_for_with_config`]. Those tools live in no pack directory,
/// so they must be surfaced through this map wherever a pack's tools are
/// resolved (persona `resolved_tool_names`, sub-agent pack scoping).
pub fn native_pack_tool_names(pack_id: &str) -> Vec<String> {
    let names: &[&str] = match pack_id {
        "digitalocean_spaces" => &[
            "spaces_list_buckets",
            "spaces_list_objects",
            "spaces_get_object",
            "spaces_put_object",
            "spaces_delete_object",
        ],
        _ => &[],
    };
    names.iter().map(|s| s.to_string()).collect()
}

/// Native tool names across every currently-enabled pack — the native-tool
/// analogue of [`http_api::HttpApiTool::installed_tool_names`]. Used to grant a
/// "full-access" sub-agent the native integration tools without naming them.
pub fn all_enabled_native_pack_tool_names() -> Vec<String> {
    crate::integration_packs::enabled_packs()
        .into_iter()
        .flat_map(|p| native_pack_tool_names(&p.manifest.id))
        .collect()
}

/// Build a `ToolCallFailed` error for a missing required parameter. Shared by
/// the meta tools so their error shape matches the native tools (e.g.
/// `write_file`).
pub(crate) fn missing_param(tool: &str, param: &str) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed {
        tool: tool.into(),
        message: format!("Missing required parameter: {param}"),
    }
}

pub fn truncate_output(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let half = max_chars / 2;
    let mut start_end = half;
    while !s.is_char_boundary(start_end) {
        start_end -= 1;
    }
    let mut tail_start = s.len() - half;
    while !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = s.len() - start_end - (s.len() - tail_start);
    format!(
        "{}\n\n... [truncated {} characters] ...\n\n{}",
        &s[..start_end],
        omitted,
        &s[tail_start..]
    )
}
