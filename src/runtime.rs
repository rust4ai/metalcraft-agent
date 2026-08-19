use metalcraft::{
    create_react_agent_with_options, AgentMessage, AgentOptions, AgentState, Executor, GraphError,
    LlmCallHook, LlmResponseHook, RunOutcome, StepGuard, ToolChoice,
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

/// The persona used when a run doesn't specify one. The orchestrator delegates
/// to specialists, so it's the correct catch-all default — a bare `coding-agent`
/// is wrong for non-coding flows (e.g. a calendar morning brief).
pub const DEFAULT_PERSONA: &str = "orchestrator-agent";

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

/// The persona to use when a caller (Workshop force-run, daemon-scheduled flow,
/// direct API) doesn't specify one. Mirrors [`configured_default_model`] so every
/// unspecified-persona path resolves the same way instead of hard-coding a slug.
/// Resolution order: `METALCRAFT_PERSONA` → `METALCRAFT_DEFAULT_PERSONA` →
/// `STARKBOT_PERSONA` (legacy) → [`DEFAULT_PERSONA`] (the orchestrator).
///
/// A flow's prompt nodes that declare their own `persona` always override this;
/// it is only the fallback for nodes that don't.
pub fn configured_default_persona() -> String {
    for key in ["METALCRAFT_PERSONA", "METALCRAFT_DEFAULT_PERSONA", "STARKBOT_PERSONA"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    DEFAULT_PERSONA.to_string()
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
    /// Whether to splice recalled memories into the turn. Read from config at
    /// construction so a long-lived CLI runner does not re-check the environment
    /// on every turn.
    recall: bool,
    /// Which conversation this runner's turns belong to, for tagging captures.
    /// Empty for one-shot and flow runs, which is fine — they are still captured,
    /// just without conversation grouping.
    capture_ctx: crate::memory::capture::CaptureContext,
    /// The agent instance whose memory this runner recalls from. `None` uses the
    /// pod-global store — the CLI and any pre-instance caller.
    instance_id: Option<String>,
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
            recall: crate::memory::recall_enabled(),
            capture_ctx: crate::memory::capture::CaptureContext::default(),
            instance_id: None,
        }
    }

    /// Bind this runner to an agent instance, so recall reads that agent's own
    /// memory rather than the pod-global store.
    pub fn with_instance(mut self, instance_id: Option<String>) -> Self {
        // Keep the capture context in step regardless of call order — a capture
        // that doesn't name its agent is material nobody can route later.
        self.capture_ctx.instance_id = instance_id.clone();
        self.instance_id = instance_id;
        self
    }

    /// Tag this runner's captures with the conversation they belong to.
    pub fn with_capture_context(
        mut self,
        chat_id: Option<String>,
        persona: Option<String>,
    ) -> Self {
        self.capture_ctx = crate::memory::capture::CaptureContext {
            chat_id,
            persona,
            instance_id: self.instance_id.clone(),
        };
        self
    }

    /// Force per-turn recall on or off, overriding the configured default.
    /// Mainly for tests and for callers that deliberately want a memoryless run.
    pub fn with_recall(mut self, recall: bool) -> Self {
        self.recall = recall;
        self
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
            Ok(Some(summary)) => {
                log::info!(
                    "Context compacted before turn -> ~{} tokens, {} messages",
                    context::estimate_tokens(&state),
                    state.messages.len()
                );
                // The summary is about to be buried in a single `Assistant`
                // message and forgotten. It is the most concentrated account of
                // this conversation that will ever exist, and the LLM call for it
                // is already paid — so hand it to memory on the way past.
                crate::memory::capture::record_compaction(&self.capture_ctx, &summary);
                true
            }
            Ok(None) => false,
            Err(e) => {
                log::warn!("Context compaction failed, proceeding uncompacted: {e}");
                false
            }
        };

        // Where this turn's messages begin, so capture can tell what was said now
        // from what was already history. Taken before injection so the synthetic
        // block does not shift the boundary.
        let turn_start = state.messages.len().saturating_sub(1);

        // Recall is spliced in AFTER compaction, so the summarizer never sees
        // (and never bakes in) a block that is about to be removed again.
        let injected = if self.recall {
            let opts = crate::memory::recall::RecallOptions {
                token_budget: Some(crate::memory::recall_token_budget()),
                instance_id: self.instance_id.clone(),
                ..Default::default()
            };
            crate::memory::inject::inject(&mut state, opts).await
        } else {
            false
        };

        let outcome = Executor::new_from_arc(self.graph.clone())
            .max_steps(self.max_steps)
            .with_step_guard(step_guard)
            .run(state, "agent")
            .await;

        // Ephemeral: the block never reaches the persisted transcript, the token
        // estimate that drives compaction, or the next turn's recall query.
        let outcome = if injected {
            outcome.map(crate::memory::inject::strip)
        } else {
            outcome
        };

        // Capture AFTER stripping, so an injected block is never mistaken for
        // something the user said and fed back into tomorrow's memories.
        if let Ok(o) = &outcome {
            capture_turn(&self.capture_ctx, turn_start, o);
        }

        (compacted, outcome)
    }
}

/// Extract this turn's exchange from the finished state and queue it for the
/// dream.
///
/// Fire-and-forget by construction: everything here is in-memory string work
/// plus one appended line, and [`crate::memory::capture`] swallows its own IO
/// errors, so a capture problem can never surface as a turn failure.
fn capture_turn(
    ctx: &crate::memory::capture::CaptureContext,
    turn_start: usize,
    outcome: &RunOutcome<AgentState>,
) {
    let state = match outcome {
        RunOutcome::Completed(s) => s,
        RunOutcome::Interrupted { state, .. } => state,
        // A failed turn still taught us what was asked and what broke, which is
        // exactly the kind of thing worth remembering.
        RunOutcome::Failed { state, .. } => state,
    };

    let recent = state.messages.get(turn_start..).unwrap_or(&[]);
    let mut user_text = String::new();
    let mut agent_text = String::new();
    let mut tools: Vec<String> = Vec::new();
    for m in recent {
        match m {
            AgentMessage::User(t) => {
                if !user_text.is_empty() {
                    user_text.push('\n');
                }
                user_text.push_str(t);
            }
            AgentMessage::Assistant(t) => {
                if !agent_text.is_empty() {
                    agent_text.push('\n');
                }
                agent_text.push_str(t);
            }
            AgentMessage::ToolCall { name, .. } if !tools.iter().any(|t| t == name) => {
                tools.push(name.clone());
            }
            _ => {}
        }
    }

    crate::memory::capture::record_turn(ctx, &user_text, &agent_text, tools);
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use crate::memory::capture::CaptureContext;

    /// Rebuild what `capture_turn` would extract, without touching the global
    /// store — the extraction logic is the part worth pinning.
    fn extract(messages: Vec<AgentMessage>, turn_start: usize) -> (String, String, Vec<String>) {
        let mut state = AgentState::new("seed");
        state.messages = messages;
        let outcome = RunOutcome::Completed(state);
        let state = match &outcome {
            RunOutcome::Completed(s) => s,
            _ => unreachable!(),
        };
        let recent = state.messages.get(turn_start..).unwrap_or(&[]);
        let mut user_text = String::new();
        let mut agent_text = String::new();
        let mut tools: Vec<String> = Vec::new();
        for m in recent {
            match m {
                AgentMessage::User(t) => {
                    if !user_text.is_empty() {
                        user_text.push('\n');
                    }
                    user_text.push_str(t);
                }
                AgentMessage::Assistant(t) => {
                    if !agent_text.is_empty() {
                        agent_text.push('\n');
                    }
                    agent_text.push_str(t);
                }
                AgentMessage::ToolCall { name, .. } => {
                    if !tools.iter().any(|t| t == name) {
                        tools.push(name.clone());
                    }
                }
                _ => {}
            }
        }
        (user_text, agent_text, tools)
    }

    fn tool_call(name: &str) -> AgentMessage {
        AgentMessage::ToolCall {
            id: format!("call-{name}"),
            call_id: None,
            name: name.to_string(),
            args: serde_json::json!({}),
        }
    }

    #[test]
    fn extraction_takes_only_this_turn_not_the_whole_history() {
        let messages = vec![
            AgentMessage::User("old question".into()),
            AgentMessage::Assistant("old answer".into()),
            AgentMessage::User("new question".into()),
            AgentMessage::Assistant("new answer".into()),
        ];
        let (user, agent, _) = extract(messages, 2);
        assert_eq!(user, "new question");
        assert_eq!(agent, "new answer");
    }

    #[test]
    fn tool_names_are_collected_in_order_and_deduplicated() {
        let messages = vec![
            AgentMessage::User("q".into()),
            tool_call("read_file"),
            tool_call("bash"),
            tool_call("read_file"),
            AgentMessage::Assistant("a".into()),
        ];
        let (_, _, tools) = extract(messages, 0);
        assert_eq!(tools, vec!["read_file", "bash"], "order preserved, no duplicates");
    }

    #[test]
    fn multiple_assistant_messages_in_one_turn_are_joined() {
        let messages = vec![
            AgentMessage::User("q".into()),
            AgentMessage::Assistant("part one".into()),
            tool_call("grep"),
            AgentMessage::Assistant("part two".into()),
        ];
        let (_, agent, tools) = extract(messages, 0);
        assert_eq!(agent, "part one\npart two");
        assert_eq!(tools, vec!["grep"]);
    }

    #[test]
    fn an_out_of_range_boundary_yields_nothing_rather_than_panicking() {
        let (user, agent, tools) = extract(vec![AgentMessage::User("q".into())], 99);
        assert!(user.is_empty() && agent.is_empty() && tools.is_empty());
    }

    #[test]
    fn a_default_capture_context_is_untagged() {
        let ctx = CaptureContext::default();
        assert!(ctx.chat_id.is_none());
        assert!(ctx.persona.is_none());
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
    /// The agent instance this run belongs to. Scheduled flow runs set it (see
    /// [`crate::flow_bindings`]) so a recurring job recalls what it did last time
    /// instead of starting cold every firing. `None` keeps the historical
    /// pod-global behaviour.
    pub instance_id: Option<String>,
    /// The roster a `sub_agent` call may reach, when this run is bound to a
    /// preset. `None` means unrestricted, as before presets existed.
    pub preset_personas: Option<Vec<String>>,
}

impl<'a> RunOneShotRequest<'a> {
    /// The common case: an unbound one-shot run.
    pub fn new(persona_slug: &'a str, cwd: &'a str, model_name: &'a str, task: &'a str) -> Self {
        Self {
            persona_slug,
            cwd,
            model_name,
            task,
            approval_mode: ApprovalMode::AutoApprove,
            diagnostics: None,
            instance_id: None,
            preset_personas: None,
        }
    }
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
    /// Live values spliced into the system prompt — currently the memory
    /// profile. Default is empty, which is right for diagnostics and for any
    /// caller that should not carry the operator's remembered context.
    pub prompt_extras: crate::persona::PromptExtras,
    /// The agent instance this turn runs as. When set, recall and capture use that
    /// agent's own two-layer memory instead of the pod-global store.
    pub instance_id: Option<String>,
    /// The active agent preset's callable roster. When set, `sub_agent` may only
    /// delegate to these personas — containment, so an installed agent cannot
    /// reach a persona its preset never declared. `None` ⇒ unscoped (any persona
    /// on the pod), which is the pre-preset behaviour and stays the default for
    /// callers that have no preset in hand.
    pub preset_personas: Option<Vec<String>>,
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
    let system_prompt =
        persona.build_system_prompt_with(&context.skills_dir, cwd, &options.prompt_extras);
    let tool_config = crate::tools::ToolConfig {
        preset_personas: options.preset_personas.clone(),
        instance_id: options.instance_id.clone(),
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
            // Reasoning is driven per-model by the inference server (it injects
            // the `reasoning` param + encrypted-content include for reasoning
            // models), so the pod leaves it unset. Reasoning items still
            // round-trip: rig captures whatever the provider returns and the
            // ReAct loop replays it. Set this only for direct-to-OpenAI use
            // without the inference gateway.
            reasoning_effort: None,
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
        RuntimeOptions {
            // free-text agent; no session reply sink
            prompt_extras: crate::persona::PromptExtras::load().await,
            instance_id: request.instance_id.clone(),
            preset_personas: request.preset_personas.clone(),
            ..Default::default()
        },
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
        .with_instance(request.instance_id.clone())
        .with_capture_context(None, Some(request.persona_slug.to_string()))
        .run(AgentState::new(request.task), step_guard)
        .await;
    outcome.map_err(|e| -> Box<dyn std::error::Error> {
        // Translate terminal inference rejections (credits/premium) into a clear
        // user-facing message; leave transient/unknown errors raw for the caller.
        let ce = classify_turn_error(&e.to_string());
        if ce.retryable {
            e.into()
        } else {
            ce.user_message.into()
        }
    })
}

/// A classified chat-turn failure, shared by every run path (one-shot task,
/// Workshop chat SSE, and the gateway/WhatsApp path). This is the single
/// vocabulary the whole error-response system speaks; see
/// `docs/CHAT_ERROR_RESPONSE_PLAN.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCode {
    /// Premium account, but out of inference credits (402). Terminal.
    InsufficientCredits,
    /// No active premium subscription (402). Terminal.
    NotPremium,
    /// Upstream provider/network failure (5xx, timeout, reset). Retryable.
    UpstreamUnavailable,
    /// Anything unrecognized. Retryable; raw text is logged to diagnostics only,
    /// never shown to the end user.
    Internal,
}

impl ErrorCode {
    /// Stable machine-readable identifier (matches the inference `code` field).
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::InsufficientCredits => "insufficient_credits",
            ErrorCode::NotPremium => "not_premium",
            ErrorCode::UpstreamUnavailable => "upstream_unavailable",
            ErrorCode::Internal => "internal",
        }
    }

    /// Whether retrying the same turn could plausibly succeed. Terminal errors
    /// (credits/premium) are `false`; the gateway path only notifies the user
    /// for terminal errors, to avoid spamming "try again" on transient blips.
    pub fn retryable(&self) -> bool {
        matches!(self, ErrorCode::UpstreamUnavailable | ErrorCode::Internal)
    }

    /// A message safe to show an end user (Workshop bubble, WhatsApp reply).
    pub fn user_message(&self) -> String {
        match self {
            ErrorCode::InsufficientCredits =>
                "You're out of Metalcraft inference credits. Top up or check your plan at \
                 https://id.metalcraftai.com/account.",
            ErrorCode::NotPremium =>
                "Metalcraft inference needs an active premium subscription. Check your plan at \
                 https://id.metalcraftai.com/account.",
            ErrorCode::UpstreamUnavailable =>
                "The AI service is temporarily unavailable — please try again in a moment.",
            ErrorCode::Internal =>
                "Something went wrong handling that message. Please try again.",
        }
        .to_string()
    }
}

/// A classified failure with its user-facing message and retry disposition.
#[derive(Debug, Clone)]
pub struct ChatError {
    pub code: ErrorCode,
    pub user_message: String,
    pub retryable: bool,
}

/// Classify the flattened error string the agent sees at the turn boundary
/// (`RunOutcome::Failed { error }` or `error_chain(Err)`). The Metalcraft
/// Inference gateway's JSON body — `{"error":"...","code":"..."}` — survives
/// intact inside rig's `ProviderError(String)` and the core's
/// `GraphError::Node.message`, so we can recover the structured `code` from the
/// text even though rig discards the HTTP status. Falls back to phrase matching
/// for pre-`code` inference and non-gateway providers.
pub fn classify_turn_error(raw: &str) -> ChatError {
    let code = classify_code(raw);
    ChatError { user_message: code.user_message(), retryable: code.retryable(), code }
}

fn classify_code(raw: &str) -> ErrorCode {
    let t = raw.to_lowercase();

    // 1. Structured `code` emitted by metalcraft-inference. The snake_case
    //    tokens are distinctive, so a substring check is a reliable proxy for
    //    parsing the JSON out of the surrounding provider-error text.
    if t.contains("insufficient_credits") {
        return ErrorCode::InsufficientCredits;
    }
    if t.contains("not_premium") {
        return ErrorCode::NotPremium;
    }

    // 2. Human phrasings (legacy / pre-`code` inference, upstream OpenAI).
    if t.contains("insufficient credits") || t.contains("out of credits") {
        return ErrorCode::InsufficientCredits;
    }
    if t.contains("requires a premium") || t.contains("premium subscription") {
        return ErrorCode::NotPremium;
    }

    // 3. Transient upstream/network failures.
    if t.contains("upstream_unavailable")
        || t.contains("bad gateway")
        || t.contains(" 502")
        || t.contains("(502")
        || t.contains("status: 502")
        || t.contains(" 503")
        || t.contains(" 504")
        || t.contains("timed out")
        || t.contains("timeout")
        || t.contains("connection reset")
        || t.contains("connection refused")
        || t.contains("dns error")
    {
        return ErrorCode::UpstreamUnavailable;
    }

    // 4. A bare 402 with no discriminator — dominant runtime cause is a premium
    //    user who ran out of credits, so surface that (both messages point to
    //    the same account page).
    if t.contains("payment_required")
        || t.contains("payment required")
        || t.contains(" 402")
        || t.contains("(402")
        || t.contains("status: 402")
    {
        return ErrorCode::InsufficientCredits;
    }

    ErrorCode::Internal
}

#[cfg(test)]
mod inference_tests {
    use super::{classify_turn_error, ErrorCode};

    fn code(s: &str) -> ErrorCode {
        classify_turn_error(s).code
    }

    #[test]
    fn detects_structured_codes() {
        assert_eq!(
            code(r#"agent: ProviderError: {"error":"insufficient credits — top up or upgrade","code":"insufficient_credits"}"#),
            ErrorCode::InsufficientCredits
        );
        assert_eq!(
            code(r#"ProviderError: {"error":"inference requires a premium subscription","code":"not_premium"}"#),
            ErrorCode::NotPremium
        );
    }

    #[test]
    fn detects_legacy_phrasings() {
        assert_eq!(code("HTTP status 402: insufficient credits: available 0"), ErrorCode::InsufficientCredits);
        assert_eq!(code("inference requires a premium subscription"), ErrorCode::NotPremium);
        // Bare 402 with no discriminator falls back to credits.
        assert_eq!(code("ProviderError { status: 402, ... }"), ErrorCode::InsufficientCredits);
    }

    #[test]
    fn transient_and_unknown() {
        assert_eq!(code("openai 502: bad gateway"), ErrorCode::UpstreamUnavailable);
        assert_eq!(code("connection reset by peer"), ErrorCode::UpstreamUnavailable);
        // Unrelated errors are Internal (raw text stays in diagnostics only).
        assert_eq!(code("400 bad request: model not found"), ErrorCode::Internal);
    }

    #[test]
    fn retry_disposition() {
        assert!(!classify_turn_error("insufficient_credits").retryable);
        assert!(!classify_turn_error("not_premium").retryable);
        assert!(classify_turn_error("502 bad gateway").retryable);
        assert!(classify_turn_error("something weird").retryable);
    }
}
