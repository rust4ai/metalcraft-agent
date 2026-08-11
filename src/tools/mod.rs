pub mod bash;
pub mod edit_file;
pub mod find_files;
pub mod gateway;
pub mod gateway_webhook;
pub mod grep;
pub mod http_api;
pub mod list_files;
pub mod load_skill;
pub mod meta_diagnostics;
pub mod meta_flow;
pub mod meta_integration;
pub mod meta_keys;
pub mod meta_persona;
pub mod meta_skill;
pub mod read_file;
pub mod say_to_user;
pub mod schedule_followup;
pub mod email_imap;
pub mod s3;
pub mod sub_agent;
pub mod twilio;
pub mod web_fetch;
pub mod write_file;

use std::path::PathBuf;
use std::sync::Arc;
use futures_util::future::BoxFuture;
use metalcraft::ToolRegistry;

/// Where a session's user-facing reply is delivered. The agent always replies
/// through the channel-agnostic `say_to_user` tool; the *caller* that builds the
/// runtime supplies this closure to route that text to the right place — the SSE
/// stream for a workshop chat, or a gateway adapter (gateway/Twilio) for a
/// gateway session. Takes the message text, returns Ok on delivery.
pub type ReplySink =
    Arc<dyn Fn(String) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// Configuration for tools that need runtime parameters.
pub struct ToolConfig {
    pub api_key: String,
    pub model_name: String,
    pub system_prompt: String,
    pub skills_dir: PathBuf,
    pub available_skills: Vec<String>,
    /// Delivery sink for the `say_to_user` tool. `None` outside a session
    /// context (e.g. one-shot/flow runs), where `say_to_user` just acks.
    pub reply_sink: Option<ReplySink>,
    /// Where a follow-up armed by `schedule_followup` in this session should be
    /// delivered when it later fires. `None` ⇒ the tool arms an unbound job
    /// (result logged only).
    pub session_binding: Option<crate::scheduled_tasks::IoBinding>,
    /// Reschedule-depth this session already carries — a follow-up scheduled
    /// here is armed at `reschedule_depth + 1` so self-rearming chains are
    /// bounded. 0 for a normal user-initiated turn.
    pub reschedule_depth: u32,
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
            "flow_set_schedules" => registry.register(meta_flow::FlowSetSchedulesTool),
            "flow_install" => registry.register(meta_flow::FlowInstallTool),
            "flow_install_dependencies" => registry.register(meta_flow::FlowInstallDependenciesTool),
            "flow_delete" => registry.register(meta_flow::FlowDeleteTool),
            "flow_run" => registry.register(meta_flow::FlowRunTool),
            "flow_resume" => registry.register(meta_flow::FlowResumeTool),
            "flow_run_status" => registry.register(meta_flow::FlowRunStatusTool),
            "flow_runs_list" => registry.register(meta_flow::FlowRunsListTool),
            "flow_templates_list" => registry.register(meta_flow::FlowTemplatesListTool),
            "flow_template_read" => registry.register(meta_flow::FlowTemplateReadTool),
            "diagnostics_list" => registry.register(meta_diagnostics::DiagnosticsListTool),
            "diagnostics_read" => registry.register(meta_diagnostics::DiagnosticsReadTool),
            // Integration packs: install (enable/disable) capabilities for the
            // agent itself, and inspect what's available + which keys they need.
            "pack_list" => registry.register(meta_integration::PackListTool),
            "pack_read" => registry.register(meta_integration::PackReadTool),
            "pack_enable" => registry.register(meta_integration::PackEnableTool),
            // API key / secret store: the secrets HTTP-API tools reference via
            // `$NAME`. Setting a key here is what lets an enabled pack authenticate.
            "key_list" => registry.register(meta_keys::KeyListTool),
            "key_set" => registry.register(meta_keys::KeySetTool),
            "key_delete" => registry.register(meta_keys::KeyDeleteTool),
            // S3-compatible object storage (AWS S3, R2, DO Spaces, MinIO, …) —
            // native tools because S3 requires per-request AWS SigV4 signing the
            // declarative HTTP-API tool can't produce. Shipped by the `s3` pack;
            // read S3_ACCESS_KEY_ID/S3_SECRET_ACCESS_KEY/S3_REGION/S3_ENDPOINT
            // from the key store.
            "s3_list_buckets" => registry.register(s3::S3ListBucketsTool),
            "s3_list_objects" => registry.register(s3::S3ListObjectsTool),
            "s3_get_object" => registry.register(s3::S3GetObjectTool),
            "s3_put_object" => registry.register(s3::S3PutObjectTool),
            "s3_delete_object" => registry.register(s3::S3DeleteObjectTool),
            // Read-only IMAP email — native tools because IMAP is not HTTP, so
            // the declarative HTTP-API tool can't speak it. Shipped by the
            // `email` pack; read IMAP_HOST/IMAP_USER/IMAP_PASSWORD(/IMAP_PORT)
            // from the key store. Every session uses EXAMINE (read-only).
            "email_list_mailboxes" => registry.register(email_imap::EmailListMailboxesTool),
            "email_search" => registry.register(email_imap::EmailSearchTool),
            "email_list_recent" => registry.register(email_imap::EmailListRecentTool),
            "email_get_message" => registry.register(email_imap::EmailGetMessageTool),
            // Generic gateway send — replies on any gateway channel (WhatsApp
            // today). Native (not a declarative HTTP-API tool) because the
            // adapters use auth schemes `$VAR` header substitution can't express
            // (e.g. Twilio's HTTP Basic auth from two key-store secrets). It
            // dispatches by the channel type's `adapter`. See `tools::gateway`.
            "gateway_send_message" => registry.register(gateway::GatewaySendMessageTool),
            "say_to_user" => registry.register(say_to_user::SayToUserTool::new(
                config.and_then(|c| c.reply_sink.clone()),
            )),
            "schedule_followup" => {
                if let Some(cfg) = config {
                    registry.register(schedule_followup::ScheduleFollowupTool::new(
                        cfg.session_binding.clone(),
                        cfg.reschedule_depth,
                    ))
                } else {
                    log::warn!("schedule_followup tool requires ToolConfig, skipping");
                    registry
                }
            }
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
/// a few — like `s3`, whose S3 SigV4 signing the HTTP-API tool
/// can't produce — ship native tools registered by name in
/// [`create_registry_for_with_config`]. Those tools live in no pack directory,
/// so they must be surfaced through this map wherever a pack's tools are
/// resolved (persona `resolved_tool_names`, sub-agent pack scoping).
pub fn native_pack_tool_names(pack_id: &str) -> Vec<String> {
    let names: &[&str] = match pack_id {
        "s3" => &[
            "s3_list_buckets",
            "s3_list_objects",
            "s3_get_object",
            "s3_put_object",
            "s3_delete_object",
        ],
        "email" => &[
            "email_list_mailboxes",
            "email_search",
            "email_list_recent",
            "email_get_message",
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

#[cfg(test)]
mod native_tools_drift {
    //! Guards that a pack's `native_tools` manifest field (the registry's source
    //! for the tool→pack index) stays in sync with [`native_pack_tool_names`]
    //! (the binary's actual native tools). If they drift, the registry would
    //! index a native tool to the wrong pack — or miss it — so a flow binding a
    //! bare `tool` node to that tool couldn't have its pack dependency resolved.
    use std::path::Path;

    #[test]
    fn seed_manifests_match_native_pack_tool_names() {
        let seed = Path::new(env!("CARGO_MANIFEST_DIR")).join("seed/integration_packs");
        let mut checked = 0;
        for entry in std::fs::read_dir(&seed).expect("seed/integration_packs must exist") {
            let manifest_path = entry.unwrap().path().join("pack.json");
            if !manifest_path.exists() {
                continue;
            }
            let m: metalcraft_packs::PackManifest =
                serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap())
                    .expect("seed pack.json must parse");
            let mut from_code = super::native_pack_tool_names(&m.id);
            let mut from_manifest = m.native_tools.clone();
            // Only packs that claim native tools on either side are relevant.
            if from_code.is_empty() && from_manifest.is_empty() {
                continue;
            }
            from_code.sort();
            from_manifest.sort();
            assert_eq!(
                from_code, from_manifest,
                "native_tools drift for seeded pack '{}': native_pack_tool_names={from_code:?} \
                 but pack.json native_tools={from_manifest:?} — update whichever is stale",
                m.id
            );
            checked += 1;
        }
        // `email` is seeded with native tools, so at least one pack must be checked;
        // a zero here means the guard silently stopped covering anything.
        assert!(checked > 0, "expected at least one seeded pack with native tools");
    }
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
