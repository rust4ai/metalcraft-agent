use metalcraft::{create_react_agent_with_hooks, AgentState, Executor, LlmCallHook, RunOutcome};
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

        let personas_dir = std::fs::canonicalize(Persona::default_personas_dir())
            .unwrap_or_else(|_| Persona::default_personas_dir());
        let skills_dir = std::fs::canonicalize(Persona::default_skills_dir())
            .unwrap_or_else(|_| Persona::default_skills_dir());
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

pub fn build_agent_runtime<M>(
    context: &AgentRuntimeContext,
    persona: &Persona,
    cwd: &str,
    model_name: &str,
    approval_mode: ApprovalMode,
    llm_call_hook: Option<LlmCallHook>,
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
    };

    let registry = crate::tools::create_registry_for_with_config(&persona.tools, Some(&tool_config));
    let client = openai::Client::new(&context.api_key)?;
    let model = client.completion_model(model_name);
    let compaction_model = make_compaction_model(&client, model_name);
    let hook = approval::build_hook(approval_mode);
    let graph = create_react_agent_with_hooks(model, registry, &system_prompt, hook, llm_call_hook)?.into_arc();

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
        |client, model_name| client.completion_model(model_name),
    )?;
    let step_guard = guard::build_agent_guard(guard::GuardConfig::default(), request.diagnostics.clone());
    let executor = Executor::new_from_arc(runtime.graph).max_steps(90).with_step_guard(step_guard);

    executor.run(AgentState::new(request.task), "agent").await.map_err(Into::into)
}
