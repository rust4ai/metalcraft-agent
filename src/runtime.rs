use metalcraft::{
    create_react_agent_with_options, AgentOptions, AgentState, Executor, GraphError, LlmCallHook,
    LlmResponseHook, RunOutcome, StepGuard, ToolChoice,
};

use crate::context::{self, CompactionConfig};

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

/// The model name to use when a caller (Workshop chat, daemon, a flow node)
/// doesn't specify one. Resolution order:
///   1. `METALCRAFT_MODEL` — set by the control plane on managed pods; typically
///      the sentinel `"default"`, which the inference gateway resolves to the
///      user's dashboard-selected default model (no pod restart needed).
///   2. `STARKBOT_MODEL` — legacy/local override.
///   3. [`DEFAULT_MODEL`] — the compile-time fallback for local/dev use.
///
/// This is the single source of truth so every unspecified-model path honours the
/// same env, rather than each site hard-coding [`DEFAULT_MODEL`]. Compaction is
/// deliberately excluded (it uses a fixed model) so it never routes through the
/// user's possibly-costlier default.
pub fn configured_default_model() -> String {
    for key in ["METALCRAFT_MODEL", "STARKBOT_MODEL"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    DEFAULT_MODEL.to_string()
}

/// Maximum executor steps for a single agent turn. Single source of truth so no
/// call site (CLI, workshop, gateway, one-shot) can silently diverge — the exact
/// class of bug [`TurnRunner`] exists to prevent.
pub const MAX_TURN_STEPS: usize = 90;

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

/// Owns the one operation every agent turn performs: **compact the context to
/// fit the window, then run the executor** (with a single, shared `max_steps`
/// and the caller's step guard).
///
/// Wraps an already-[built](build_agent_runtime) runtime so the turn body lives
/// in exactly one place. Historically the CLI, workshop, gateway, and one-shot
/// paths each hand-wired this sequence inline, and a behaviour present in one
/// (context compaction) went missing from another — the bug this type prevents.
///
/// Construction supports both runtime lifetimes the callers need:
/// - **build once, reuse** — the CLI builds a `TurnRunner` and calls
///   [`run`](TurnRunner::run) each turn, rebuilding only on persona/model/cwd
///   switch. No per-turn graph/client rebuild.
/// - **build per turn** — the daemon constructs a `TurnRunner`, runs one turn,
///   and drops it, matching its spawn-per-turn session model.
///
/// The **step guard is a `run` parameter, not a field**, because guard lifetime
/// is genuinely caller-specific: the CLI reuses one session-long guard, while
/// the workshop/gateway guard is per-turn (it captures that turn's SSE/reply
/// sender to emit tool events). Keeping it out of the struct lets both hold the
/// runtime the way they need without forcing a guard-lifetime choice on either.
pub struct TurnRunner<M: CompletionModel + 'static> {
    graph: SharedAgentGraph,
    compaction_model: M,
    compaction_config: CompactionConfig,
    max_steps: usize,
}

impl<M: CompletionModel + 'static> TurnRunner<M> {
    /// Wrap a freshly built runtime with default per-turn knobs
    /// ([`CompactionConfig::default`], [`MAX_TURN_STEPS`]).
    pub fn new(runtime: BuiltAgentRuntime<M>) -> Self {
        Self {
            graph: runtime.graph,
            compaction_model: runtime.compaction_model,
            compaction_config: CompactionConfig::default(),
            max_steps: MAX_TURN_STEPS,
        }
    }

    /// Compact `state` if it exceeds the window, then run one turn to completion
    /// under `step_guard`.
    ///
    /// Compaction is best-effort: a failure is logged and the turn proceeds with
    /// the uncompacted state rather than being dropped. Returns whether
    /// compaction ran alongside the outcome so an interactive caller (the CLI)
    /// can surface it; daemon callers ignore the flag and rely on the log line.
    pub async fn run(
        &self,
        mut state: AgentState,
        step_guard: StepGuard<AgentState>,
    ) -> (bool, Result<RunOutcome<AgentState>, GraphError>) {
        let compacted = match context::compact_if_needed(
            &mut state,
            &self.compaction_model,
            &self.compaction_config,
        )
        .await
        {
            Ok(true) => {
                log::info!(
                    "Context compacted before turn -> ~{} tokens, {} messages",
                    context::estimate_tokens(&state),
                    state.messages.len()
                );
                true
            }
            Ok(false) => false,
            Err(e) => {
                log::warn!("Context compaction failed, proceeding uncompacted: {e}");
                false
            }
        };

        let outcome = Executor::new_from_arc(self.graph.clone())
            .max_steps(self.max_steps)
            .with_step_guard(step_guard)
            .run(state, "agent")
            .await;

        (compacted, outcome)
    }
}

/// Build a rig OpenAI client that honors `OPENAI_BASE_URL` — so a pod can route
/// all inference through the Metalcraft Inference gateway (auth + credit metering
/// via the injected `METALCRAFT_TOKEN`). `openai::Client::new` ignores the base URL;
/// this mirrors rig's own `from_env` (builder + optional `base_url`).
///
/// Returns rig 0.38's **default** client, which targets OpenAI's **Responses API**
/// and POSTs to `{base}/responses`. We deliberately do NOT use `.completions_api()`
/// here: the chat/completions surface strictly requires every assistant
/// `tool_calls` message to be immediately followed by its tool responses, but the
/// agent's per-call message layout serializes parallel tool calls as separate
/// assistant messages (see the note in the `metalcraft` crate,
/// `docs/PARALLEL_TOOL_CALL_ORPHANS.md`), which chat/completions rejects with a 400
/// ("tool_call_ids did not have response messages"). The Responses API tolerates
/// that layout, so the agent works. The gateway implements `POST {base}/responses`
/// as a passthrough (see metalcraft-inference `controllers::responses`), so routing
/// through it still bills credits.
pub fn build_openai_client(api_key: &str) -> Result<openai::Client, Box<dyn std::error::Error>> {
    let mut builder = openai::Client::builder().api_key(api_key);
    if let Ok(base) = std::env::var("OPENAI_BASE_URL") {
        let base = base.trim();
        if !base.is_empty() {
            builder = builder.base_url(base);
        }
    }
    Ok(builder.build()?)
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
    let client = build_openai_client(&context.api_key)?;
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

    // Route through the shared turn primitive so one-shot runs get the same
    // compaction + max-steps wiring as every other path (this was previously the
    // one turn path with no compaction). The compaction flag is irrelevant for a
    // single-shot task, so it's discarded.
    let (_compacted, outcome) = TurnRunner::new(runtime)
        .run(AgentState::new(request.task), step_guard)
        .await;
    outcome.map_err(|e| -> Box<dyn std::error::Error> {
        match out_of_credits_message(&e.to_string()) {
            Some(msg) => msg.into(),
            None => e.into(),
        }
    })
}

/// Turn the Metalcraft Inference gateway's credit/premium rejection (a 402 from
/// `inference.metalcraftai.com`) into a clear user-facing message, instead of a raw
/// provider error. Returns `None` for any other error. Reusable across run paths.
pub fn out_of_credits_message(err_text: &str) -> Option<String> {
    let t = err_text.to_lowercase();
    let hit = t.contains("insufficient credits")
        || t.contains("payment required")
        || t.contains("requires a premium")
        || t.contains("out of credits")
        || t.contains(" 402")
        || t.contains("(402")
        || t.contains("status: 402");
    hit.then(|| {
        "You're out of Metalcraft inference credits (or your premium subscription \
         lapsed). Top up or check your plan at https://id.metalcraftai.com/account."
            .to_string()
    })
}

#[cfg(test)]
mod inference_tests {
    use super::out_of_credits_message;

    #[test]
    fn detects_gateway_credit_and_premium_rejections() {
        assert!(out_of_credits_message("HTTP status 402: insufficient credits: available 0").is_some());
        assert!(out_of_credits_message("ProviderError { status: 402, ... }").is_some());
        assert!(out_of_credits_message("inference requires a premium subscription").is_some());
        // Unrelated errors pass through unchanged.
        assert!(out_of_credits_message("connection reset by peer").is_none());
        assert!(out_of_credits_message("400 bad request: model not found").is_none());
    }
}
