use async_trait::async_trait;
use metalcraft::{AgentState, Executor, RunOutcome, create_react_agent};
use rig::client::CompletionClient;

pub struct SubAgentTool {
    api_key: String,
    model_name: String,
    system_prompt: String,
    /// Personas this sub-agent may run as, from the active agent preset's roster.
    /// `None` ⇒ unscoped (any persona on the pod) — the pre-preset behaviour.
    preset_personas: Option<Vec<String>>,
    /// The agent instance the parent turn runs as. A delegated subtask remembers
    /// into the same place, rather than opening a second store nobody reads.
    instance_id: Option<String>,
}

impl SubAgentTool {
    /// Restrict delegation to an agent preset's callable roster.
    pub fn with_preset_personas(mut self, personas: Option<Vec<String>>) -> Self {
        self.preset_personas = personas;
        self
    }

    /// Inherit the parent turn's agent identity.
    pub fn with_instance(mut self, instance_id: Option<String>) -> Self {
        self.instance_id = instance_id;
        self
    }

    pub fn new(api_key: String, model_name: String, system_prompt: String) -> Self {
        Self {
            api_key,
            model_name,
            system_prompt,
            preset_personas: None,
            instance_id: None,
        }
    }
}

/// The roster, short enough to read. A preset that delegates to any installed
/// persona can have a roster of a hundred slugs, and pasting all of them into an
/// error the model has to parse buries the one thing it needs: which names are legal.
fn summarize_roster(roster: &[String]) -> String {
    const SHOWN: usize = 24;
    if roster.len() <= SHOWN {
        return roster.join(", ");
    }
    format!(
        "{}, and {} more",
        roster[..SHOWN].join(", "),
        roster.len() - SHOWN
    )
}

/// The integrations `persona` declares that this pod does not have installed.
///
/// Split out from the delegation guard below so the case that broke it stays
/// testable without an LLM in the loop: an agent pack vendors its integrations
/// into the content store rather than `<data>/integrations/`, and a check that
/// missed that layout refused every agent-pack persona as "not installed" while
/// that persona's tools were resolving perfectly well.
pub fn missing_integrations(persona: &crate::persona::Persona) -> Vec<String> {
    persona
        .integrations
        .iter()
        .filter(|p| !crate::integrations::is_enabled(p))
        .cloned()
        .collect()
}

#[async_trait]
impl metalcraft::Tool for SubAgentTool {
    fn name(&self) -> &str {
        "sub_agent"
    }

    fn description(&self) -> &str {
        "Spawn a sub-agent to handle an independent subtask. Sub-agents run autonomously \
         with their own tool set and return a result. Use this for research, exploration, \
         or any task that can be delegated. Multiple sub-agents run concurrently."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        // With a preset roster in hand, restrict `persona` to an enum so the model
        // cannot even propose a persona the preset never declared — the same trick
        // `load_skill` uses for its skill list.
        let persona_schema = match &self.preset_personas {
            Some(roster) if !roster.is_empty() => serde_json::json!({
                "type": "string",
                "enum": roster,
                "description": "Run the sub-agent AS one of this agent's personas. It inherits that persona's tools (including integration tools it is scoped to via its packs), system prompt and skills. When set, `tool_set`/`pack` are ignored."
            }),
            _ => serde_json::json!({
                "type": "string",
                "description": "Run the sub-agent AS a named persona (e.g. 'linear-agent', 'github-agent'). The sub-agent inherits that persona's tools — including any integration tools the persona is scoped to via its packs — plus its system prompt and skills. This is the preferred way to delegate an integration task: pick the persona built for that service rather than assembling raw tools. When set, `tool_set`/`pack` are ignored."
            }),
        };
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task for the sub-agent to perform"
                },
                "persona": persona_schema,
                "tool_set": {
                    "type": "string",
                    "enum": ["read_only", "full", "all"],
                    "description": "Tool set for the sub-agent. 'read_only' (default) = read_file, list_files, grep, find_files. 'full' = adds write_file, edit_file, bash. 'all' = 'full' plus integration tools (e.g. the starflask_* media tools) — use this to delegate tasks that call an external service or integration."
                },
                "pack": {
                    "type": "string",
                    "description": "Only meaningful with tool_set='all'. Scope the integration tools to a single installed pack by id (e.g. 'github', 'linear', 'starflask') so the sub-agent gets just that one integration's tools instead of every installed integration. Omit to grant all installed integration tools."
                }
            },
            "required": ["task"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let task = args["task"]
            .as_str()
            .ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
                tool: "sub_agent".into(),
                message: "Missing required parameter: task".into(),
            })?;

        // Two ways to scope the sub-agent:
        //   1. `persona` — run AS a named persona: its resolved tools (incl.
        //      pack-scoped integration tools), its system prompt, its skills.
        //      Preferred for integration work (e.g. persona "linear-agent").
        //   2. otherwise `tool_set` (read_only/full/all) [+ `pack`], using the
        //      parent's system prompt.
        let persona_slug = args["persona"].as_str().filter(|s| !s.is_empty());

        // Containment. The schema enum guides the model; this is the rule. An agent
        // must not be able to reach a persona its preset never declared.
        if let (Some(slug), Some(roster)) = (persona_slug, self.preset_personas.as_ref()) {
            if !roster.iter().any(|p| p == slug) {
                return Err(metalcraft::GraphError::ToolCallFailed {
                    tool: "sub_agent".into(),
                    message: format!(
                        "persona '{slug}' is not in this agent's roster ({}). Delegate to one of those, or handle the task directly.",
                        summarize_roster(roster)
                    ),
                });
            }
        }

        let (registry, sub_prompt) = if let Some(slug) = persona_slug {
            let persona = crate::persona::Persona::load(slug, &crate::paths::personas_dir())
                .map_err(|e| metalcraft::GraphError::ToolCallFailed {
                    tool: "sub_agent".into(),
                    message: format!("Failed to load persona '{slug}': {e}"),
                })?;

            // Fail fast if the persona depends on integrations that aren't
            // enabled. Otherwise its pack-scoped tools resolve to nothing, the
            // model calls a tool that isn't registered, and the dropped call
            // leaves an orphaned assistant tool_call the OpenAI API rejects with
            // an opaque 400. A clear, actionable error here is far better.
            let missing = missing_integrations(&persona);
            if !missing.is_empty() {
                return Ok(serde_json::json!({
                    "error": true,
                    "result": format!(
                        "Persona '{slug}' requires integration(s) {missing:?} that are not \
                         installed, so its tools are unavailable. Install the agent pack that \
                         provides them (agentpack_install), then retry."
                    ),
                }));
            }

            let base_prompt = persona.build_system_prompt(&crate::paths::skills_dir(), ".");
            let config = crate::tools::ToolConfig {
                // A nested sub-agent must not widen its own reach.
                preset_personas: None,
                instance_id: self.instance_id.clone(),
                api_key: self.api_key.clone(),
                model_name: self.model_name.clone(),
                system_prompt: base_prompt.clone(),
                skills_dir: crate::paths::skills_dir(),
                available_skills: persona.skills.clone(),
                // A sub-agent has no user-facing channel of its own; its result
                // is returned to the parent, not delivered via say_to_user.
                reply_sink: None,
                // Nor does it inherit a scheduling binding — a follow-up armed
                // from inside a sub-agent is unbound.
                session_binding: None,
                reschedule_depth: 0,
            };
            let registry = crate::tools::create_registry_for_with_config(
                &persona.resolved_tool_names(),
                Some(&config),
            );
            let sub_prompt = format!(
                "{base_prompt}\n\nYou are a sub-agent. Complete the given task efficiently and \
                 report your findings. Be concise in your final answer."
            );
            (registry, sub_prompt)
        } else {
            let tool_set = args["tool_set"].as_str().unwrap_or("read_only");

            let mut tool_names: Vec<String> = match tool_set {
                "full" | "all" => vec![
                    "read_file",
                    "write_file",
                    "edit_file",
                    "bash",
                    "list_files",
                    "grep",
                    "find_files",
                ],
                _ => vec!["read_file", "list_files", "grep", "find_files"],
            }
            .into_iter()
            .map(String::from)
            .collect();

            // "all" additionally grants integration (HTTP-API) tools — e.g. the
            // starflask_* media tools — so an orchestrator can delegate "use
            // starflask to generate an image" without naming the exact tool. An
            // optional `pack` scopes this to a single integration (e.g. only the
            // github_* tools) instead of every installed one.
            if tool_set == "all" {
                use crate::tools::http_api::HttpApiTool;
                let integration_tools = match args["pack"].as_str() {
                    Some(pack) if !pack.is_empty() => {
                        let mut t = HttpApiTool::installed_tool_names_for_integration(pack);
                        // Native-tool packs (e.g. s3) ship no
                        // api_tools/ files, so add their tools from the registry.
                        t.extend(crate::tools::native_integration_tool_names(pack));
                        t
                    }
                    _ => {
                        let mut t = HttpApiTool::installed_tool_names();
                        t.extend(crate::tools::all_enabled_native_integration_tool_names());
                        t
                    }
                };
                for name in integration_tools {
                    if !tool_names.contains(&name) {
                        tool_names.push(name);
                    }
                }
            }

            let registry = crate::tools::create_registry_for(&tool_names);
            let sub_prompt = format!(
                "{}\n\nYou are a sub-agent. Complete the given task efficiently and report your \
                 findings. Be concise in your final answer.",
                self.system_prompt
            );
            (registry, sub_prompt)
        };

        // Route through the same gateway-aware client the main runtime uses — this
        // honors OPENAI_BASE_URL (so sub-agent inference is billed through the
        // Metalcraft Inference gateway) and uses the Responses API, which tolerates
        // the agent's parallel-tool-call message layout (see build_openai_client).
        let client = crate::runtime::build_openai_client(&self.api_key).map_err(|e| {
            metalcraft::GraphError::ToolCallFailed {
                tool: "sub_agent".into(),
                message: format!("Failed to create OpenAI client: {e}"),
            }
        })?;
        let model = client.completion_model(&self.model_name);

        let graph = create_react_agent(model, registry, &sub_prompt).map_err(|e| {
            metalcraft::GraphError::ToolCallFailed {
                tool: "sub_agent".into(),
                message: format!("Failed to build sub-agent graph: {e}"),
            }
        })?;

        let executor = Executor::new(graph).max_steps(90);

        // Run with timeout to prevent runaway sub-agents
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            executor.run(AgentState::new(task), "sub-agent"),
        )
        .await;

        match result {
            Ok(Ok(RunOutcome::Completed(state))) => {
                let answer = state.final_answer().unwrap_or("(no answer)").to_string();
                let tools_used = state.tools_called();
                let turn_count = state.turns().len();
                Ok(serde_json::json!({
                    "result": answer,
                    "tools_used": tools_used,
                    "turns": turn_count,
                }))
            }
            Ok(Ok(RunOutcome::Interrupted { reason, .. })) => Ok(serde_json::json!({
                "result": format!("Sub-agent interrupted: {reason}"),
                "error": true,
            })),
            Ok(Ok(RunOutcome::Failed { node, error, .. })) => Ok(serde_json::json!({
                "result": format!("Sub-agent failed at {node}: {error}"),
                "error": true,
            })),
            Ok(Err(e)) => Ok(serde_json::json!({
                "result": format!("Sub-agent error: {e}"),
                "error": true,
            })),
            Err(_) => Ok(serde_json::json!({
                "result": "Sub-agent timed out after 120 seconds",
                "error": true,
            })),
        }
    }
}
