pub mod bash;
pub mod edit_file;
pub mod find_files;
pub mod grep;
pub mod http_api;
pub mod list_files;
pub mod load_skill;
pub mod read_file;
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
