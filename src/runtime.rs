use metalcraft::{
    create_react_agent_with_options, AgentOptions, AgentState, Executor, LlmCallHook,
    LlmResponseHook, RunOutcome, ToolChoice,
};

use crate::tools::ReplySink;
use rig::client::CompletionClient;
use rig::completion::CompletionModel;
use rig::providers::openai;

use crate::approval::{self, ApprovalMode};
use crate::diagnostics::DiagnosticsLogger;
use crate::guard;
use crate::persona::Persona;

use std::path::PathBuf;
use std::sync::Arc;

pub const DEFAULT_MODEL: &str = "gpt-5.4";
pub const AVAILABLE_MODELS: &[&str] = &["gpt-5.4-mini", "gpt-5.4", "gpt-5.5"];

pub struct AgentRuntimeContext {
    pub personas_dir: PathBuf,
    pub skills_dir: PathBuf,
    pub api_key: String,
}

pub type SharedAgentGraph = Arc<metalcraft::CompiledGraph<AgentState>>;

pub struct BuiltAgentRuntime<M: CompletionModel + 'static> {
    pub graph: SharedAgentGraph,
    pub compaction_model: M,
}

impl AgentRuntimeContext {
    pub fn from_environment() -> Result<Self, Box<dyn std::error::Error>> {
        dotenvy::dotenv().ok();

        let personas_dir = crate::paths::personas_dir();
        let skills_dir = crate::paths::skills_dir();
        let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| {
            "OPENAI_API_KEY environment variable not set. Add it to your .env file or export it."
        })?;

        Ok(Self {
            personas_dir,
            skills_dir,
            api_key,
        })
    }
}

pub struct RunOneShotRequest<'a> {
    pub persona_slug: &'a str,
    pub cwd: &'a str,
    pub model_name: &'a str,
    pub task: &'a str,
    pub approval_mode: ApprovalMode,
    pub diagnostics: Option<Arc<DiagnosticsLogger>>,
}

/// Per-run I/O wiring that varies by session preset. `default()` reproduces the
/// historical free-text agent (model decides text-vs-tool; a free-text answer
/// ends the turn) — used by the CLI and one-shot/flow runs. Workshop and gateway
/// sessions instead force tool-only output and reply through `say_to_user`.
#[derive(Default)]
pub struct RuntimeOptions {
    /// Delivery sink for the `say_to_user` tool (SSE for workshop, adapter send
    /// for gateway). `None` ⇒ `say_to_user` just acks.
    pub reply_sink: Option<ReplySink>,
    /// Tool-choice policy. `Required` forces tool-only output.
    pub tool_choice: ToolChoice,
    /// Tools that end the turn when called. For tool-only sessions this is
    /// `["say_to_user"]`; the tool is auto-injected into the registry if the
    /// persona doesn't already list it.
    pub terminal_tools: Vec<String>,
    /// Delivery binding for follow-ups armed via `schedule_followup` in this
    /// session (workshop chat / gateway). `None` ⇒ unbound (result logged).
    pub session_binding: Option<crate::scheduled_tasks::IoBinding>,
    /// Reschedule depth this session already carries (see [`crate::tools::ToolConfig`]).
    pub reschedule_depth: u32,
}

pub fn build_agent_runtime<M>(
    context: &AgentRuntimeContext,
    persona: &Persona,
    cwd: &str,
    model_name: &str,
    approval_mode: ApprovalMode,
    llm_call_hook: Option<LlmCallHook>,
    llm_response_hook: Option<LlmResponseHook>,
    options: RuntimeOptions,
    make_compaction_model: impl FnOnce(&openai::Client, &str) -> M,
) -> Result<BuiltAgentRuntime<M>, Box<dyn std::error::Error>>
where
    M: CompletionModel + 'static,
{
    let system_prompt = persona.build_system_prompt(&context.skills_dir, cwd);
    let tool_config = crate::tools::ToolConfig {
        api_key: context.api_key.clone(),
        model_name: model_name.to_string(),
        system_prompt: system_prompt.clone(),
        skills_dir: context.skills_dir.clone(),
        available_skills: persona.skills.clone(),
        reply_sink: options.reply_sink,
        session_binding: options.session_binding,
        reschedule_depth: options.reschedule_depth,
    };

    // Resolve the persona's full tool set (explicit tools + any pack-scoped
    // integration tools) — this is what the registry and step guard see. Any
    // terminal tool (e.g. `say_to_user`) the session needs is injected here even
    // if the persona didn't list it, so tool-only mode always has a way to end.
    let mut resolved_tools = persona.resolved_tool_names();
    for terminal in &options.terminal_tools {
        if !resolved_tools.iter().any(|t| t == terminal) {
            resolved_tools.push(terminal.clone());
        }
    }
    let registry = crate::tools::create_registry_for_with_config(&resolved_tools, Some(&tool_config));
    let client = openai::Client::new(&context.api_key)?;
    let model = client.completion_model(model_name);
    let compaction_model = make_compaction_model(&client, model_name);
    let hook = approval::build_hook(approval_mode);
    let graph = create_react_agent_with_options(
        model,
        registry,
        &system_prompt,
        AgentOptions {
            before_tool_call: hook,
            llm_call_hook,
            llm_response_hook,
            tool_choice: options.tool_choice,
            terminal_tools: options.terminal_tools,
        },
    )?
    .into_arc();

    Ok(BuiltAgentRuntime {
        graph,
        compaction_model,
    })
}

pub async fn run_one_shot_task(
    context: &AgentRuntimeContext,
    request: RunOneShotRequest<'_>,
) -> Result<RunOutcome<AgentState>, Box<dyn std::error::Error>> {
    let persona = Persona::load(request.persona_slug, &context.personas_dir)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let llm_call_hook: Option<LlmCallHook> = request.diagnostics.as_ref().map(|d| {
        let logger = d.clone();
        Arc::new(move |snapshot: &metalcraft::LlmCallSnapshot| {
            logger.log_llm_request(snapshot);
        }) as LlmCallHook
    });

    let runtime = build_agent_runtime(
        context,
        &persona,
        request.cwd,
        request.model_name,
        request.approval_mode.clone(),
        llm_call_hook,
        None, // one-shot runs don't emit OTLP traces
        RuntimeOptions::default(), // free-text agent; no session reply sink
        |client, model_name| client.completion_model(model_name),
    )?;
    // Let the guard know which of this persona's tools are status polls, so
    // repeatedly checking an async job isn't mistaken for a runaway loop.
    let guard_config = guard::GuardConfig {
        poll_tools: crate::tools::http_api::HttpApiTool::poll_tool_names(&persona.resolved_tool_names()),
        ..guard::GuardConfig::default()
    };
    let step_guard = guard::build_agent_guard(guard_config, request.diagnostics.clone());
    let executor = Executor::new_from_arc(runtime.graph).max_steps(90).with_step_guard(step_guard);

    executor.run(AgentState::new(request.task), "agent").await.map_err(Into::into)
}
