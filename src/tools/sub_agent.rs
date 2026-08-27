use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use metalcraft::{AgentState, Executor, GuardAction, RunOutcome, StepGuard, create_react_agent};
use rig::client::CompletionClient;

/// How long a delegated sub-agent may run before it is cut off.
///
/// A runaway sub-agent burning tokens unattended is the thing this guards, so it
/// stays bounded — but the bound cannot be a constant. Delegation to a persona
/// that waits on real provisioning (a buildr.space workspace reaches `ready` in
/// one to two minutes) needs longer than one that edits a file, and a sub-agent
/// killed mid-wait looks exactly like an agent that failed the task.
///
/// A persona may declare its own bound (`max_run_secs`) — the pack author knows
/// the work — and `SUB_AGENT_TIMEOUT_SECS` overrides both, because the operator
/// paying for the tokens gets the last word. Anything unparseable or zero is
/// ignored in favour of the default, because a typo must not disable the guard,
/// and everything is clamped to [`MAX_SUB_AGENT_TIMEOUT_SECS`] so a persona
/// cannot declare its way out of being bounded at all.
const DEFAULT_SUB_AGENT_TIMEOUT_SECS: u64 = 120;

/// Ceiling on any declared or configured delegation timeout: half an hour.
const MAX_SUB_AGENT_TIMEOUT_SECS: u64 = 1800;

fn sub_agent_timeout(persona_max_run_secs: Option<u64>) -> std::time::Duration {
    let configured = crate::key_store::lookup("SUB_AGENT_TIMEOUT_SECS")
        .and_then(|v| v.trim().parse::<u64>().ok());
    std::time::Duration::from_secs(resolve_timeout_secs(configured, persona_max_run_secs))
}

/// The decision itself, split out from reading the key store so it can be tested
/// without an ambient environment.
fn resolve_timeout_secs(configured: Option<u64>, persona_max_run_secs: Option<u64>) -> u64 {
    configured
        .filter(|v| *v > 0)
        .or(persona_max_run_secs.filter(|v| *v > 0))
        .unwrap_or(DEFAULT_SUB_AGENT_TIMEOUT_SECS)
        .min(MAX_SUB_AGENT_TIMEOUT_SECS)
}

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
    /// The parent turn's stop flag — the one the chat's stop button sets.
    ///
    /// Delegation is the one tool call that is itself a whole agent run, so
    /// without this a stop pressed during it lands nowhere: the parent's step
    /// guard cannot fire until the tool returns, and the sub-agent runs on to
    /// its step limit or its timeout, spending the whole way. Sharing the flag
    /// is what makes the promise "stop stops the agent" true through a
    /// delegation rather than only outside one.
    interrupt: Option<Arc<AtomicBool>>,
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

    /// Share the parent turn's stop flag, so pressing stop ends the delegated
    /// run too. `None` ⇒ nothing can stop this delegation early (a flow run, a
    /// one-shot task: nobody is watching a button).
    pub fn with_interrupt(mut self, interrupt: Option<Arc<AtomicBool>>) -> Self {
        self.interrupt = interrupt;
        self
    }

    pub fn new(api_key: String, model_name: String, system_prompt: String) -> Self {
        Self {
            api_key,
            model_name,
            system_prompt,
            preset_personas: None,
            instance_id: None,
            interrupt: None,
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

impl SubAgentTool {
    /// Has the parent turn been asked to stop?
    fn stopped(&self) -> bool {
        self.interrupt
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
    }

    /// The parent turn's stop button, as a guard the nested executor can hold.
    ///
    /// Same contract as the chat's own guard: checked at step boundaries, so the
    /// step in flight finishes and the sub-agent stops between steps rather than
    /// mid-call. `None` when there is no flag to watch — a flow node or a
    /// one-shot run has no button behind it, and an always-continue guard would
    /// only cost a closure per step.
    fn stop_guard(&self) -> Option<StepGuard<AgentState>> {
        let flag = self.interrupt.clone()?;
        Some(Arc::new(move |_state: &AgentState, _ev| {
            if flag.load(Ordering::Relaxed) {
                GuardAction::Stop("Stopped by the user.".into())
            } else {
                GuardAction::Continue
            }
        }))
    }
}

#[async_trait]
impl metalcraft::Tool for SubAgentTool {
    fn name(&self) -> &str {
        "sub_agent"
    }

    fn description(&self) -> &str {
        "Spawn a sub-agent to handle an independent subtask. Sub-agents run autonomously \
         with their own tool set and return a result. Use this for research, exploration, \
         or any task that can be delegated. Sub-agents run one at a time, in the order \
         you call them, so a turn costs the sum of its delegations."
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
        // Already stopped before this delegation began — one LLM call can return
        // several tool calls, and they all run inside the one node the guard has
        // not been asked about yet. Starting a whole agent run there would be the
        // most expensive way to ignore the button.
        if self.stopped() {
            return Ok(serde_json::json!({
                "result": "Delegation not started: stopped by the user.",
                "stopped": true,
                "error": true,
            }));
        }

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

        // A persona's own bound on how long delegating to it may run; `None` for
        // the ad-hoc tool_set path, which has no persona to ask.
        let mut persona_max_run_secs: Option<u64> = None;
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

            persona_max_run_secs = persona.max_run_secs;

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
                // A sub-agent that delegates again is still this turn: the stop
                // has to reach all the way down, not just one level.
                interrupt: self.interrupt.clone(),
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

        let mut executor = Executor::new(graph).max_steps(90);

        // The parent turn's stop button, reaching into the delegated run.
        if let Some(guard) = self.stop_guard() {
            executor = executor.with_step_guard(guard);
        }

        // Run with timeout to prevent runaway sub-agents
        let timeout = sub_agent_timeout(persona_max_run_secs);
        let result =
            tokio::time::timeout(timeout, executor.run(AgentState::new(task), "sub-agent")).await;

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
            Ok(Ok(RunOutcome::Interrupted { state, reason, .. })) => {
                // A user stop is not a delegation that went wrong. Say so plainly,
                // and hand back whatever the sub-agent had reached: the parent's
                // own guard ends the turn on the next step either way, and a
                // resumed conversation should not pretend the work never happened.
                if self.stopped() {
                    let partial = state.final_answer().unwrap_or("").to_string();
                    return Ok(serde_json::json!({
                        "result": if partial.is_empty() {
                            "Delegation stopped by the user before the sub-agent answered.".to_string()
                        } else {
                            format!("Delegation stopped by the user. Partial result: {partial}")
                        },
                        "stopped": true,
                        "error": true,
                    }));
                }
                Ok(serde_json::json!({
                    "result": format!("Sub-agent interrupted: {reason}"),
                    "error": true,
                }))
            }
            Ok(Ok(RunOutcome::Failed { node, error, .. })) => Ok(serde_json::json!({
                "result": format!("Sub-agent failed at {node}: {error}"),
                "error": true,
            })),
            Ok(Err(e)) => Ok(serde_json::json!({
                "result": format!("Sub-agent error: {e}"),
                "error": true,
            })),
            Err(_) => Ok(serde_json::json!({
                "result": format!("Sub-agent timed out after {} seconds", timeout.as_secs()),
                "error": true,
            })),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_declared_uses_the_default() {
        assert_eq!(resolve_timeout_secs(None, None), DEFAULT_SUB_AGENT_TIMEOUT_SECS);
    }

    #[test]
    fn a_persona_that_knows_it_is_slow_gets_longer() {
        // Provisioning a remote workspace takes one to two minutes before the
        // delegate can do anything at all; the default would kill it mid-wait.
        assert_eq!(resolve_timeout_secs(None, Some(900)), 900);
    }

    #[test]
    fn the_operator_overrides_the_persona() {
        assert_eq!(resolve_timeout_secs(Some(300), Some(900)), 300);
    }

    #[test]
    fn zero_and_garbage_fall_back_rather_than_disabling_the_guard() {
        assert_eq!(resolve_timeout_secs(Some(0), None), DEFAULT_SUB_AGENT_TIMEOUT_SECS);
        assert_eq!(resolve_timeout_secs(Some(0), Some(900)), 900);
        assert_eq!(resolve_timeout_secs(None, Some(0)), DEFAULT_SUB_AGENT_TIMEOUT_SECS);
    }

    /// A stop pressed before the delegation starts must not start it. This is the
    /// cheap half of the guarantee; the other half is the step guard below, which
    /// ends a sub-agent already running.
    #[tokio::test]
    async fn a_stopped_turn_does_not_start_a_delegation() {
        use metalcraft::Tool;
        let flag = Arc::new(AtomicBool::new(true));
        let tool = SubAgentTool::new("k".into(), "gpt-5.4".into(), "p".into())
            .with_interrupt(Some(flag.clone()));
        let out = tool
            .call(serde_json::json!({"task": "count to a million"}))
            .await
            .expect("the tool answers rather than failing");
        assert_eq!(out["stopped"], true, "{out}");
        assert!(
            out["result"].as_str().unwrap().contains("stopped by the user"),
            "the parent needs to read why it got nothing: {out}"
        );
    }

    /// The other half: a delegation already running ends at the sub-agent's next
    /// step boundary, which is what the nested executor's guard is for.
    #[test]
    fn a_running_delegation_is_stopped_at_the_next_step() {
        let flag = Arc::new(AtomicBool::new(false));
        let tool = SubAgentTool::new("k".into(), "m".into(), "p".into())
            .with_interrupt(Some(flag.clone()));
        let guard = tool.stop_guard().expect("a flag means a guard");
        let state = AgentState::new("count".to_string());
        let event = metalcraft::StepEvent {
            node: "agent".into(),
            next: "tools".into(),
            duration: std::time::Duration::from_millis(1),
            outcome: metalcraft::StepOutcome::Success,
        };
        assert!(
            matches!(guard(&state, &event), GuardAction::Continue),
            "nothing pressed: the sub-agent runs"
        );
        flag.store(true, Ordering::Relaxed);
        assert!(
            matches!(guard(&state, &event), GuardAction::Stop(_)),
            "stop pressed: the sub-agent stops instead of running on to its step limit"
        );
    }

    /// Without a flag, delegation is unstoppable by construction — flows and
    /// one-shot runs have no button — and must not be short-circuited.
    #[test]
    fn no_flag_means_nothing_is_stopped() {
        let tool = SubAgentTool::new("k".into(), "m".into(), "p".into());
        assert!(!tool.stopped());
        assert!(tool.stop_guard().is_none());
        let running = SubAgentTool::new("k".into(), "m".into(), "p".into())
            .with_interrupt(Some(Arc::new(AtomicBool::new(false))));
        assert!(!running.stopped());
    }

    #[test]
    fn no_declaration_can_escape_the_ceiling() {
        assert_eq!(resolve_timeout_secs(None, Some(u64::MAX)), MAX_SUB_AGENT_TIMEOUT_SECS);
        assert_eq!(resolve_timeout_secs(Some(u64::MAX), None), MAX_SUB_AGENT_TIMEOUT_SECS);
    }
}
