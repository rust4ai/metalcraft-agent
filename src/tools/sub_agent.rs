use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use metalcraft::{
    AgentOptions, AgentState, Executor, GuardAction, RunOutcome, StepGuard, ToolChoice,
    ToolRegistry, create_react_agent_with_options,
};
use rig::client::CompletionClient;

use crate::tools::sub_agent_registry as delegates;
use crate::tools::yield_result::{HandoffReport, ReportSlot, YIELD_TOOL_NAME, YieldResultTool};

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

/// How deep a delegation tree may go: an orchestrator (depth 0) delegates to a
/// worker (depth 1), and that worker may delegate once more (depth 2).
///
/// A bound has to exist. Until now nothing counted the levels, so the only thing
/// stopping a tree from recursing was that each level eventually timed out —
/// which is a bound on *one branch*, not on the tree, and every extra level
/// multiplies spend. Two is the shallowest depth that still allows the one shape
/// that genuinely needs it: an orchestrator handing a workspace agent a job that
/// the workspace agent then splits.
pub const MAX_SUB_AGENT_DEPTH: u32 = 2;

/// How many delegations may run at once in one batch.
///
/// Each one is a whole agent spending its own tokens, so this is a spend
/// multiplier before it is a throughput one. Three is enough for the shape this
/// exists for — survey three areas, read three files, review three angles —
/// without a single tool call quietly costing five times what the model expects.
pub const MAX_PARALLEL_DELEGATES: usize = 3;

/// How many times a child may end a turn without calling `yield_result` before
/// the parent stops asking.
///
/// One reminder catches the ordinary miss — the model wrote a tidy prose
/// summary out of habit. The second exists because a model that ignored the
/// first reminder is usually mid-plan rather than confused, and one more nudge
/// is far cheaper than throwing the whole run away. The third attempt is not a
/// nudge at all: it hands the child a registry with nothing in it but the
/// terminal tool, so the contract is the only move left (see
/// [`SubAgentTool::child_registry`]). Past that the parent reconciles prose and
/// says so, rather than spending a fourth run to learn the same thing.
const MAX_YIELD_ATTEMPTS: u32 = 3;

/// Steps one attempt of a delegated run may take.
const SUB_AGENT_MAX_STEPS: usize = 90;

/// Tools that change the workspace rather than read it.
///
/// A batch runs its delegates *at the same time* in **one** workspace, so two of
/// them writing it would overwrite each other silently — the worst kind of bug
/// to go looking for. The batch path allows at most one writer: one writer
/// alongside readers is only imprecise (a reader may see a file mid-edit), while
/// two writers is corruption. Unknown tools count as safe: a pack tool that
/// calls somebody else's API is not touching this repo.
const WORKSPACE_WRITING_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "bash",
    "buildr_write_file",
    "buildr_exec",
    "buildr_git",
    "buildr_build",
    "buildr_test",
    "buildr_serve",
];

/// Whether one batch entry would write the workspace.
///
/// `tool_set` says so directly — `full` and `all` both add `write_file`,
/// `edit_file` and `bash`. A named persona is asked what tools it actually
/// resolves to; a persona that cannot be loaded is assumed to write, because
/// guessing "safe" about something unreadable is how the rule gets defeated.
fn entry_writes_workspace(entry: &serde_json::Value) -> bool {
    if let Some(slug) = entry["persona"].as_str().filter(|s| !s.is_empty()) {
        return match crate::persona::Persona::load(slug, &crate::paths::personas_dir()) {
            Ok(p) => p
                .resolved_tool_names()
                .iter()
                .any(|t| WORKSPACE_WRITING_TOOLS.contains(&t.as_str())),
            Err(_) => true,
        };
    }
    matches!(entry["tool_set"].as_str(), Some("full") | Some("all"))
}

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

/// Appended to every sub-agent's system prompt.
///
/// It describes the termination contract rather than *being* it: the contract is
/// the `yield_result` tool's JSON Schema, and the child physically cannot end
/// its turn any other way (see [`crate::tools::yield_result`]). What is left to
/// say in prose is the part a schema cannot carry — that an honest `not_done`
/// is worth more than a tidy answer, and that the child has none of the parent's
/// conversation to fall back on.
const SUB_AGENT_PROMPT_SUFFIX: &str = "\n\nYou are a sub-agent. Complete the given task \
efficiently. Be concise.\n\n\
You have NONE of the parent conversation — only the task above. If it is missing something \
you need, say so in your result rather than guessing.\n\n\
Your turn ends ONLY when you call `yield_result`. Free text is not delivered anywhere: whatever \
you would have written as a final answer goes in that tool's `completed` field. List every \
outstanding part of the task in `not_done` — including work you could not do because you lack \
the tools for it — and name the persona best suited to finish it in `suggest_persona` if you \
know one.";

/// Sent to a child that ended a turn without meeting the contract.
const YIELD_REMINDER: &str = "Your turn ended without calling `yield_result`, so NOTHING was \
delivered to the agent that delegated this — prose is not a result. Call `yield_result` now: \
`completed` is your answer (what you did and what you found, concretely), and `not_done` lists \
anything still outstanding ([] if nothing is).";

/// Sent on the final attempt, where the child's registry holds nothing else.
const YIELD_LAST_CALL: &str = "This is the last chance to deliver anything at all. Every other \
tool has been withdrawn — `yield_result` is the only call available. Summarise what you have, \
however partial, in `completed`, and list the rest in `not_done`.";

pub struct SubAgentTool {
    api_key: String,
    model_name: String,
    system_prompt: String,
    /// Personas this sub-agent may run as, from the active agent preset's roster.
    /// `None` ⇒ unscoped (any persona on the pod) — the pre-preset behaviour.
    preset_personas: Option<Vec<String>>,
    /// How deep this delegation already is; a child runs at `depth + 1`.
    depth: u32,
    /// The agent instance the parent turn runs as. A delegated subtask remembers
    /// into the same place, rather than opening a second store nobody reads.
    /// It is also what scopes the delegate registry: two agent instances on one
    /// pod address their own delegates, never each other's.
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
    /// The parent turn's plan. A delegation that comes back reporting unfinished
    /// work records a handoff here, which stops the parent from closing the turn
    /// until it has acted on it. `None` ⇒ nobody is tracking obligations (a flow
    /// node, a one-shot run, a nested sub-agent).
    turn_plan: Option<crate::turn_plan::SharedTurnPlan>,
}

impl SubAgentTool {
    /// Set how deep in a delegation tree this tool sits.
    pub fn with_depth(mut self, depth: u32) -> Self {
        self.depth = depth;
        self
    }

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

    /// Report unfinished delegations into the parent turn's plan.
    pub fn with_turn_plan(mut self, plan: Option<crate::turn_plan::SharedTurnPlan>) -> Self {
        self.turn_plan = plan;
        self
    }

    pub fn new(api_key: String, model_name: String, system_prompt: String) -> Self {
        Self {
            api_key,
            model_name,
            system_prompt,
            preset_personas: None,
            depth: 0,
            instance_id: None,
            interrupt: None,
            turn_plan: None,
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
    pub(crate) fn stopped(&self) -> bool {
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

    /// Which delegate directory this turn's children belong to.
    ///
    /// The agent instance, because that is the identity a follow-up is asked in
    /// the name of: the same agent, later in the same conversation or the next
    /// one, addressing a delegate it spawned. A pod-global scope would let one
    /// agent revive another's delegate; a per-turn scope would make every
    /// delegate unaddressable the moment the turn it was born in ended, which
    /// is the whole behaviour this replaces. `None` (the CLI, a flow) falls back
    /// to one shared scope — there is no instance to key on, and those callers
    /// are single-tenant by construction.
    pub(crate) fn scope(&self) -> String {
        self.instance_id
            .clone()
            .unwrap_or_else(|| "pod".to_string())
    }
}

/// Everything needed to build and run one child, resolved once so a revival can
/// rebuild the same child from the arguments that created it.
pub(crate) struct ChildPlan {
    /// What to call this delegate: the persona slug, or the ad-hoc tool set.
    pub(crate) label: String,
    /// The child's whole system prompt, contract suffix included.
    prompt: String,
    timeout: Duration,
    tool_names: Vec<String>,
    /// Persona children carry a nested [`crate::tools::ToolConfig`] so their own
    /// delegation stays inside the roster and their skills resolve; an ad-hoc
    /// `tool_set` child is a flat list of workspace tools and needs none.
    nested: Option<NestedChild>,
}

struct NestedChild {
    base_prompt: String,
    skills: Vec<String>,
}

/// Why a delegation never started.
pub(crate) enum Refusal {
    /// Answer the model with this tool result: something it can fix.
    Reply(serde_json::Value),
    /// A hard tool failure: containment was violated, or a required input is
    /// missing.
    Fail(metalcraft::GraphError),
}

/// How one child's run ended.
pub(crate) enum ChildOutcome {
    Settled {
        report: HandoffReport,
        /// The child never met the contract; `report` was reconciled out of its
        /// trailing prose.
        unreconciled: bool,
        messages: Vec<metalcraft::AgentMessage>,
        transcript: String,
        tools_used: Vec<String>,
        turns: usize,
    },
    /// Terminal. The operator stopped it, or the clock ran out — there is
    /// either a decision not to undo or nothing left to resume from.
    Killed {
        reason: String,
        /// A user stop rather than a failure, which the parent reports
        /// differently.
        stopped: bool,
    },
    /// Not terminal. Something intact is left, so the delegate parks and a
    /// follow-up can pick it up.
    Suspended { reason: String, transcript: String },
}

#[async_trait]
impl metalcraft::Tool for SubAgentTool {
    fn name(&self) -> &str {
        "sub_agent"
    }

    fn description(&self) -> &str {
        "Spawn a sub-agent to handle an independent subtask. Sub-agents run autonomously \
         with their own tool set and return a result. Use this for research, exploration, \
         or any task that can be delegated.\n\n\
         Pass `task` for one delegation. Pass `tasks` (up to 3) to run several AT THE SAME TIME \
         — three surveys take as long as the slowest one rather than all three added up. A batch \
         is mostly for READING: research, review, survey. At most one entry may change the \
         workspace, because there is one workspace and two agents editing it at once overwrite \
         each other.\n\n\
         Every delegation returns a `delegate_id`. The delegate stays addressable afterwards: \
         `sub_agent_send` asks it a follow-up using everything it already read (far cheaper than \
         a fresh delegation), and `sub_agent_read` pages through its full transcript."
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
                    "description": "The task for the sub-agent to perform. Omit when using `tasks`."
                },
                "tasks": {
                    "type": "array",
                    "maxItems": MAX_PARALLEL_DELEGATES,
                    "description": "Several independent delegations to run at the same time (max 3). Each entry takes the same fields as a single call. At most one of them may change the workspace — the rest must be read-only.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "task": { "type": "string" },
                            "persona": { "type": "string" },
                            "tool_set": { "type": "string", "enum": ["read_only", "full", "all"] },
                            "pack": { "type": "string" }
                        },
                        "required": ["task"]
                    }
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
        match args["tasks"].as_array().filter(|b| !b.is_empty()) {
            Some(batch) => self.run_batch(batch).await,
            None => self.run_one(&args).await,
        }
    }
}

impl SubAgentTool {
    /// Run a batch of delegations at the same time.
    ///
    /// The win is wall-clock: three surveys that each take ninety seconds take
    /// ninety seconds instead of four and a half minutes, and a tick that would
    /// have overrun its budget doing them one after another fits.
    ///
    /// Two rules, both enforced here rather than asked for in a prompt:
    /// **at most [`MAX_PARALLEL_DELEGATES`]**, because each delegate spends its
    /// own tokens and one tool call should not quietly cost five; and **at most
    /// one delegate that writes the workspace**, because there is one workspace
    /// and two agents editing it at once overwrite each other invisibly.
    async fn run_batch(
        &self,
        batch: &[serde_json::Value],
    ) -> metalcraft::Result<serde_json::Value> {
        // A batch of one is not a batch. Route it down the single path so the
        // rules below — which exist because things run *at the same time* —
        // never refuse something that has nothing to run alongside.
        if let [only] = batch {
            return self.run_one(only).await;
        }
        if batch.len() > MAX_PARALLEL_DELEGATES {
            return Ok(serde_json::json!({
                "error": true,
                "result": format!(
                    "{} delegations at once; the limit is {MAX_PARALLEL_DELEGATES}. Each one \
                     spends its own tokens. Run the most useful {MAX_PARALLEL_DELEGATES} now \
                     and the rest after.",
                    batch.len()
                ),
            }));
        }
        let writers: Vec<String> = batch
            .iter()
            .filter(|e| entry_writes_workspace(e))
            .map(|e| {
                e["persona"]
                    .as_str()
                    .or_else(|| e["tool_set"].as_str())
                    .unwrap_or("one of them")
                    .to_string()
            })
            .collect();
        if writers.len() > 1 {
            return Ok(serde_json::json!({
                "error": true,
                "result": format!(
                    "{} of these delegations change the workspace ({}), and there is only one \
                     workspace — two agents editing it at the same time overwrite each other \
                     without either noticing. Run at most one writing delegation at a time; the \
                     reading ones can go together.",
                    writers.len(),
                    writers.join(", ")
                ),
            }));
        }

        let results = futures_util::future::join_all(batch.iter().map(|entry| async move {
            match self.run_one(entry).await {
                Ok(v) => v,
                // One delegate failing is a fact about that delegate, not about
                // the batch: the others' work is still worth returning.
                Err(e) => serde_json::json!({ "error": true, "result": format!("{e}") }),
            }
        }))
        .await;

        Ok(serde_json::json!({
            "results": results,
            "count": batch.len(),
        }))
    }

    /// Run exactly one delegation. The trait's `call` is a thin router over
    /// this: one task runs it once, a batch runs it several times at once.
    async fn run_one(&self, args: &serde_json::Value) -> metalcraft::Result<serde_json::Value> {
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

        if self.depth >= MAX_SUB_AGENT_DEPTH {
            return Ok(serde_json::json!({
                "error": true,
                "result": format!(
                    "Delegation is {} levels deep already, which is the limit. Do this part \
                     yourself, or report back what is left so the agent above you can route it.",
                    self.depth
                ),
            }));
        }

        let task = args["task"]
            .as_str()
            .ok_or_else(|| metalcraft::GraphError::ToolCallFailed {
                tool: "sub_agent".into(),
                message: "Missing required parameter: task".into(),
            })?
            .to_string();

        let plan = match self.prepare_child(args) {
            Ok(plan) => plan,
            Err(Refusal::Reply(value)) => return Ok(value),
            Err(Refusal::Fail(error)) => return Err(error),
        };

        // The id is allocated before the run, not after it. A result that can
        // only name the delegate on success cannot name it on the timeout —
        // which is exactly when the parent needs to say which one died.
        let id = delegates::open(&self.scope(), &plan.label, &task, spawn_args(args));

        let outcome = self.run_child(&plan, AgentState::new(task)).await;
        Ok(self.settle_outcome(&id, &plan, outcome))
    }

    /// Resolve a delegation request into a runnable child.
    ///
    /// Two ways to scope one:
    ///   1. `persona` — run AS a named persona: its resolved tools (incl.
    ///      pack-scoped integration tools), its system prompt, its skills.
    ///      Preferred for integration work (e.g. persona "linear-agent").
    ///   2. otherwise `tool_set` (read_only/full/all) [+ `pack`], using the
    ///      parent's system prompt.
    ///
    /// Separate from the run so a revival rebuilds the *same* child from the
    /// arguments that created it — and re-passes every containment check on the
    /// way, because a roster that changed since the first run must bind the
    /// second one too.
    pub(crate) fn prepare_child(&self, args: &serde_json::Value) -> Result<ChildPlan, Refusal> {
        let persona_slug = args["persona"].as_str().filter(|s| !s.is_empty());

        // Containment. The schema enum guides the model; this is the rule. An agent
        // must not be able to reach a persona its preset never declared.
        if let (Some(slug), Some(roster)) = (persona_slug, self.preset_personas.as_ref())
            && !roster.iter().any(|p| p == slug)
        {
            return Err(Refusal::Fail(metalcraft::GraphError::ToolCallFailed {
                tool: "sub_agent".into(),
                message: format!(
                    "persona '{slug}' is not in this agent's roster ({}). Delegate to one of those, or handle the task directly.",
                    summarize_roster(roster)
                ),
            }));
        }

        let Some(slug) = persona_slug else {
            let tool_set = args["tool_set"].as_str().unwrap_or("read_only");
            return Ok(ChildPlan {
                label: format!("tool_set:{tool_set}"),
                prompt: format!("{}{SUB_AGENT_PROMPT_SUFFIX}", self.system_prompt),
                timeout: sub_agent_timeout(None),
                tool_names: ad_hoc_tool_names(tool_set, args["pack"].as_str()),
                nested: None,
            });
        };

        let persona = crate::persona::Persona::load(slug, &crate::paths::personas_dir()).map_err(
            |e| {
                Refusal::Fail(metalcraft::GraphError::ToolCallFailed {
                    tool: "sub_agent".into(),
                    message: format!("Failed to load persona '{slug}': {e}"),
                })
            },
        )?;

        // Fail fast if the persona depends on integrations that aren't
        // enabled. Otherwise its pack-scoped tools resolve to nothing, the
        // model calls a tool that isn't registered, and the dropped call
        // leaves an orphaned assistant tool_call the OpenAI API rejects with
        // an opaque 400. A clear, actionable error here is far better.
        let missing = missing_integrations(&persona);
        if !missing.is_empty() {
            return Err(Refusal::Reply(serde_json::json!({
                "error": true,
                "result": format!(
                    "Persona '{slug}' requires integration(s) {missing:?} that are not \
                     installed, so its tools are unavailable. Install the agent pack that \
                     provides them (agentpack_install), then retry."
                ),
            })));
        }

        let base_prompt = persona.build_system_prompt(&crate::paths::skills_dir(), ".");
        Ok(ChildPlan {
            label: slug.to_string(),
            prompt: format!("{base_prompt}{SUB_AGENT_PROMPT_SUFFIX}"),
            timeout: sub_agent_timeout(persona.max_run_secs),
            tool_names: persona.resolved_tool_names(),
            nested: Some(NestedChild {
                base_prompt,
                skills: persona.skills.clone(),
            }),
        })
    }

    /// The child's tool registry for one attempt.
    ///
    /// `forced` is the final attempt, and it withdraws everything else. With
    /// [`ToolChoice::Required`] already forbidding free text, a registry holding
    /// only the terminal tool leaves the child exactly one legal move — which is
    /// what pi achieves by pinning `toolChoice` to the yield tool, expressed
    /// through the knob this library actually has.
    fn child_registry(&self, plan: &ChildPlan, slot: ReportSlot, forced: bool) -> ToolRegistry {
        if forced {
            return ToolRegistry::new().register(YieldResultTool::new(slot));
        }
        let registry = match &plan.nested {
            Some(nested) => crate::tools::create_registry_for_with_config(
                &plan.tool_names,
                Some(&crate::tools::ToolConfig {
                    // A nested sub-agent must not widen its own reach — and
                    // `None` here used to mean exactly that, because `None` is
                    // *unscoped* (see the roster check above, which only runs
                    // when there is a roster). A preset-restricted agent could
                    // therefore delegate to a delegate that reached any persona
                    // on the pod. The roster travels down instead.
                    preset_personas: self.preset_personas.clone(),
                    sub_agent_depth: self.depth + 1,
                    instance_id: self.instance_id.clone(),
                    api_key: self.api_key.clone(),
                    model_name: self.model_name.clone(),
                    system_prompt: nested.base_prompt.clone(),
                    skills_dir: crate::paths::skills_dir(),
                    available_skills: nested.skills.clone(),
                    // A sub-agent has no user-facing channel of its own; its
                    // result is returned to the parent via `yield_result`, not
                    // delivered via say_to_user.
                    reply_sink: None,
                    // Nor does it inherit a scheduling binding — a follow-up
                    // armed from inside a sub-agent is unbound.
                    session_binding: None,
                    reschedule_depth: 0,
                    // A sub-agent that delegates again is still this turn: the
                    // stop has to reach all the way down, not just one level.
                    interrupt: self.interrupt.clone(),
                    // The plan belongs to the parent turn. A sub-agent must not
                    // be able to satisfy it (by writing steps it never did) or
                    // inherit obligations it cannot see, so delegation stops
                    // here and the nested run answers on its own terms.
                    turn_plan: None,
                    // Same reasoning as the plan, and it matters more here: the
                    // scratchpad is the project's entire memory, and a delegate
                    // holds only the fragment of the task it was handed. One
                    // that rewrote the document would be summarising a project
                    // it cannot see. It reports back instead, and the tick —
                    // which can see all of it — writes.
                    project_id: None,
                }),
            ),
            None => crate::tools::create_registry_for(&plan.tool_names),
        };
        registry.register(YieldResultTool::new(slot))
    }

    /// Run a child until it meets the termination contract, or until the parent
    /// gives up on it.
    ///
    /// The whole delegation shares one deadline rather than each attempt getting
    /// a fresh timeout: three reminders must not be able to turn a 120-second
    /// bound into a six-minute one.
    pub(crate) async fn run_child(
        &self,
        plan: &ChildPlan,
        initial: AgentState,
    ) -> ChildOutcome {
        let slot = crate::tools::yield_result::slot();
        let deadline = Instant::now() + plan.timeout;
        let mut state = initial;

        for attempt in 1..=MAX_YIELD_ATTEMPTS {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return ChildOutcome::Killed {
                    reason: format!("timed out after {} seconds", plan.timeout.as_secs()),
                    stopped: false,
                };
            }
            let forced = attempt == MAX_YIELD_ATTEMPTS;

            // Route through the same gateway-aware client the main runtime uses —
            // this honors OPENAI_BASE_URL (so sub-agent inference is billed through
            // the Metalcraft Inference gateway) and uses the Responses API, which
            // tolerates the agent's parallel-tool-call message layout (see
            // build_openai_client).
            let client = match crate::runtime::build_openai_client(&self.api_key) {
                Ok(client) => client,
                Err(e) => {
                    return ChildOutcome::Killed {
                        reason: format!("could not create the OpenAI client: {e}"),
                        stopped: false,
                    };
                }
            };
            let model = client.completion_model(&self.model_name);
            let registry = self.child_registry(plan, slot.clone(), forced);

            let graph = match create_react_agent_with_options(
                model,
                registry,
                &plan.prompt,
                AgentOptions {
                    // Free text is not a way this turn can end. Paired with the
                    // terminal tool below, the loop reaches END only through a
                    // successful `yield_result`.
                    tool_choice: ToolChoice::Required,
                    terminal_tools: vec![YIELD_TOOL_NAME.to_string()],
                    ..Default::default()
                },
            ) {
                Ok(graph) => graph,
                Err(e) => {
                    return ChildOutcome::Killed {
                        reason: format!("could not build the sub-agent graph: {e}"),
                        stopped: false,
                    };
                }
            };

            let mut executor = Executor::new(graph).max_steps(SUB_AGENT_MAX_STEPS);
            // The parent turn's stop button, reaching into the delegated run.
            if let Some(guard) = self.stop_guard() {
                executor = executor.with_step_guard(guard);
            }

            let before = state.clone();
            match tokio::time::timeout(remaining, executor.run(state, "sub-agent")).await {
                Err(_) => {
                    // The run future is dropped mid-flight, so there is no state
                    // to hand back — which is precisely why a timeout is
                    // terminal rather than parkable.
                    return ChildOutcome::Killed {
                        reason: format!("timed out after {} seconds", plan.timeout.as_secs()),
                        stopped: false,
                    };
                }
                Ok(Err(e)) => {
                    return ChildOutcome::Suspended {
                        reason: format!("sub-agent error: {e}"),
                        transcript: delegates::render_transcript(&before.messages),
                    };
                }
                Ok(Ok(RunOutcome::Failed { state, node, error })) => {
                    return ChildOutcome::Suspended {
                        reason: format!("sub-agent failed at {node}: {error}"),
                        transcript: delegates::render_transcript(&state.messages),
                    };
                }
                Ok(Ok(RunOutcome::Interrupted { state, reason, .. })) => {
                    // A user stop is a decision, not a fault: it is terminal and
                    // must not offer a revival that would quietly undo it.
                    // Anything else left intact state behind and parks.
                    return if self.stopped() {
                        ChildOutcome::Killed {
                            reason: "stopped by the user".to_string(),
                            stopped: true,
                        }
                    } else {
                        ChildOutcome::Suspended {
                            reason: format!("sub-agent interrupted: {reason}"),
                            transcript: delegates::render_transcript(&state.messages),
                        }
                    };
                }
                Ok(Ok(RunOutcome::Cancelled { state, .. })) => {
                    // Cooperative cancellation hands back intact state that
                    // somebody meant to come back to. Parking keeps that
                    // promise; aborting would throw the work away and then
                    // refuse to let anyone ask for it again.
                    return ChildOutcome::Suspended {
                        reason: "sub-agent cancelled".to_string(),
                        transcript: delegates::render_transcript(&state.messages),
                    };
                }
                Ok(Ok(RunOutcome::Completed(completed))) => {
                    state = completed;
                    let yielded = slot
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    if let Some(report) = yielded {
                        return settled(report, false, state);
                    }
                    if forced {
                        break;
                    }
                    log::warn!(
                        "sub-agent '{}' ended attempt {attempt} without calling {YIELD_TOOL_NAME}",
                        plan.label
                    );
                    state = continue_from(
                        state.messages,
                        if attempt + 1 == MAX_YIELD_ATTEMPTS {
                            YIELD_LAST_CALL
                        } else {
                            YIELD_REMINDER
                        },
                    );
                }
            }
        }

        // Every attempt spent and the contract still unmet. Reconciling the
        // child's trailing prose is better than returning nothing — it did the
        // work — but it is reported as reconciled, not as a report, because the
        // one thing the contract exists to establish (what is *not* done) was
        // never said and must not be assumed empty.
        let prose = state
            .final_answer()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("(the sub-agent produced no final answer)")
            .to_string();
        settled(
            HandoffReport {
                completed: prose,
                not_done: Vec::new(),
                suggest_persona: None,
            },
            true,
            state,
        )
    }

    /// File a finished run in the delegate directory and describe it to the
    /// parent.
    ///
    /// The parent's copy is deliberately bounded: it carries a preview plus the
    /// id, and the full transcript stays in the registry behind
    /// `sub_agent_read`. A tool result is replayed into every later LLM request,
    /// so an unbounded one costs the parent's context for the rest of the
    /// conversation.
    pub(crate) fn settle_outcome(
        &self,
        id: &str,
        plan: &ChildPlan,
        outcome: ChildOutcome,
    ) -> serde_json::Value {
        let scope = self.scope();
        match outcome {
            ChildOutcome::Settled {
                report,
                unreconciled,
                messages,
                transcript,
                tools_used,
                turns,
            } => {
                let preview = delegates::preview_of(&report.completed);
                let complete = report.is_complete();
                delegates::settle(
                    &scope,
                    id,
                    delegates::Outcome {
                        summary: report.completed.clone(),
                        not_done: report.not_done.clone(),
                        unreconciled,
                        tools_used: tools_used.clone(),
                        turns,
                        transcript,
                        messages,
                    },
                );

                let mut out = serde_json::json!({
                    "delegate_id": id,
                    "delegate_state": delegates::DelegateState::Idle.label(),
                    "result": preview.text,
                    "tools_used": tools_used,
                    "turns": turns,
                    "completed": complete,
                    "follow_up": format!(
                        "`sub_agent_send` with id '{id}' continues this delegate with everything \
                         it already read; `sub_agent_read` pages its full transcript."
                    ),
                });
                if preview.truncated {
                    out["truncated"] = serde_json::json!(true);
                    out["full_result_bytes"] = serde_json::json!(preview.full_bytes);
                }
                if unreconciled {
                    out["unreconciled"] = serde_json::json!(true);
                    out["result_note"] = serde_json::json!(format!(
                        "This delegate never called `{YIELD_TOOL_NAME}` despite \
                         {MAX_YIELD_ATTEMPTS} attempts, so the text above was reconciled from its \
                         prose and it never said what it left undone. Treat completeness as \
                         unverified: check the work, or ask it directly with `sub_agent_send`."
                    ));
                }
                if !complete {
                    out["not_done"] = serde_json::json!(report.not_done);
                    if let Some(next) = &report.suggest_persona {
                        out["suggest_persona"] = serde_json::json!(next);
                    }
                    // Record the obligation where the reply tool will see it. A
                    // delegation that reported unfinished work now holds the turn
                    // open until the orchestrator does something about it. The
                    // handoff is filed under the delegate *id*, so the gate's
                    // message names something the model can address directly.
                    if let Some(plan_handle) = &self.turn_plan {
                        crate::turn_plan::lock(plan_handle).record_handoff(
                            crate::turn_plan::Handoff {
                                from: id.to_string(),
                                not_done: report.not_done,
                                suggest_persona: report.suggest_persona,
                            },
                        );
                    }
                }
                out
            }
            ChildOutcome::Killed { reason, stopped } => {
                delegates::abort(&scope, id, &reason);
                serde_json::json!({
                    "delegate_id": id,
                    "delegate_state": delegates::DelegateState::Aborted.label(),
                    "error": true,
                    "stopped": stopped,
                    "result": format!(
                        "Delegation to '{}' ended: {reason}. It cannot be revived — start a \
                         fresh delegation if the work still needs doing.",
                        plan.label
                    ),
                })
            }
            ChildOutcome::Suspended { reason, transcript } => {
                delegates::park(&scope, id, &reason, &transcript);
                serde_json::json!({
                    "delegate_id": id,
                    "delegate_state": delegates::DelegateState::Parked.label(),
                    "error": true,
                    "result": format!(
                        "Delegation to '{}' did not finish: {reason}. What it reached is kept — \
                         `sub_agent_read` with id '{id}' shows it, and `sub_agent_send` picks it \
                         back up.",
                        plan.label
                    ),
                })
            }
        }
    }
}

/// Wrap a report and the state that produced it as a settled outcome.
fn settled(report: HandoffReport, unreconciled: bool, state: AgentState) -> ChildOutcome {
    ChildOutcome::Settled {
        report,
        unreconciled,
        transcript: delegates::render_transcript(&state.messages),
        tools_used: state
            .tools_called()
            .into_iter()
            .filter(|t| t != YIELD_TOOL_NAME)
            .collect(),
        turns: state.turns().len(),
        messages: state.messages,
    }
}

/// Re-open a child's turn with one more user message on the end of its history.
///
/// Both the yield reminder and a revival follow-up are the same move: keep
/// everything the child accumulated, append a prompt, let it run again. Built
/// by splicing a fresh single-message state rather than by naming
/// `AgentMessage::User`'s payload type, so this survives the variant carrying a
/// `String` in one metalcraft release and a structured input in the next.
pub(crate) fn continue_from(
    mut messages: Vec<metalcraft::AgentMessage>,
    text: &str,
) -> AgentState {
    let mut next = AgentState::new(text.to_string());
    messages.append(&mut next.messages);
    next.messages = messages;
    next.pending_tool_calls.clear();
    next.is_done = false;
    next
}

/// The fields that define a delegation, kept so a revival can rebuild the same
/// child. Deliberately not the whole `args`: `tasks` (the batch envelope) would
/// make a revival re-run the entire batch.
fn spawn_args(args: &serde_json::Value) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    for key in ["task", "persona", "tool_set", "pack"] {
        if let Some(value) = args.get(key).filter(|v| !v.is_null()) {
            out.insert(key.to_string(), value.clone());
        }
    }
    serde_json::Value::Object(out)
}

/// The tool list behind `tool_set` (+ optional `pack`).
fn ad_hoc_tool_names(tool_set: &str, pack: Option<&str>) -> Vec<String> {
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
        let integration_tools = match pack.filter(|p| !p.is_empty()) {
            Some(pack) => {
                let mut t = HttpApiTool::installed_tool_names_for_integration(pack);
                // Native-tool packs (e.g. s3) ship no api_tools/ files, so add
                // their tools from the registry.
                t.extend(crate::tools::native_integration_tool_names(pack));
                t
            }
            None => {
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
    tool_names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delegate_cannot_reach_past_its_parents_roster() {
        // The bug this guards: the roster check is skipped when
        // `preset_personas` is None, and a nested delegate used to be handed
        // None — so a preset-restricted agent could delegate to a delegate that
        // reached anything on the pod. The roster has to travel down.
        let tool = SubAgentTool::new("k".into(), "m".into(), "p".into())
            .with_preset_personas(Some(vec!["research-agent".into()]));
        assert_eq!(
            tool.preset_personas.as_deref(),
            Some(["research-agent".to_string()].as_slice())
        );
        // The roster is enforced on every path into a child, including a
        // revival: a persona removed from the roster since the first run must
        // not come back through the follow-up door.
        let refused = tool.prepare_child(&serde_json::json!({
            "task": "x",
            "persona": "not-on-the-roster"
        }));
        assert!(
            matches!(refused, Err(Refusal::Fail(_))),
            "containment is a rule, not a hint"
        );
    }

    #[test]
    fn a_delegation_tree_has_a_bottom() {
        // Depth is what bounds the tree. Without it the only limit is that each
        // branch eventually times out, which bounds a branch, not the spend.
        assert_eq!(MAX_SUB_AGENT_DEPTH, 2);
        let at_limit =
            SubAgentTool::new("k".into(), "m".into(), "p".into()).with_depth(MAX_SUB_AGENT_DEPTH);
        assert!(at_limit.depth >= MAX_SUB_AGENT_DEPTH);
        let root = SubAgentTool::new("k".into(), "m".into(), "p".into());
        assert_eq!(root.depth, 0, "a turn a person started is the top");
    }

    #[test]
    fn a_batch_keeps_writers_out() {
        // Two delegates editing one workspace at the same time overwrite each
        // other and neither notices, so the batch path refuses writers outright
        // rather than trusting a prompt to keep them out.
        assert!(!entry_writes_workspace(
            &serde_json::json!({ "task": "read it" })
        ));
        assert!(!entry_writes_workspace(
            &serde_json::json!({ "task": "read it", "tool_set": "read_only" })
        ));
        assert!(entry_writes_workspace(
            &serde_json::json!({ "task": "fix it", "tool_set": "full" })
        ));
        assert!(entry_writes_workspace(
            &serde_json::json!({ "task": "fix it", "tool_set": "all" })
        ));
        // A persona nobody can load is assumed to write: guessing "safe" about
        // something unreadable is how the rule gets defeated.
        assert!(entry_writes_workspace(
            &serde_json::json!({ "task": "x", "persona": "no-such-persona" })
        ));
    }

    #[test]
    fn a_batch_of_one_is_not_a_batch() {
        // The rules below exist because entries run *at the same time*. A lone
        // entry has nothing to run alongside, so refusing it for writing the
        // workspace would be refusing an ordinary delegation for no reason.
        let writer = serde_json::json!({ "task": "fix it", "tool_set": "full" });
        assert!(entry_writes_workspace(&writer));
        // (run_batch routes a single entry to run_one before any rule applies —
        // exercised through the project task-dispatch test, which does not need a
        // live model to reach the guards.)
    }

    #[test]
    fn a_batch_is_bounded() {
        // Each delegate spends its own tokens, so one tool call must not be
        // able to quietly cost five.
        assert_eq!(MAX_PARALLEL_DELEGATES, 3);
    }

    #[test]
    fn nothing_declared_uses_the_default() {
        assert_eq!(
            resolve_timeout_secs(None, None),
            DEFAULT_SUB_AGENT_TIMEOUT_SECS
        );
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
        assert_eq!(
            resolve_timeout_secs(Some(0), None),
            DEFAULT_SUB_AGENT_TIMEOUT_SECS
        );
        assert_eq!(resolve_timeout_secs(Some(0), Some(900)), 900);
        assert_eq!(
            resolve_timeout_secs(None, Some(0)),
            DEFAULT_SUB_AGENT_TIMEOUT_SECS
        );
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
            out["result"]
                .as_str()
                .unwrap()
                .contains("stopped by the user"),
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
        assert_eq!(
            resolve_timeout_secs(None, Some(u64::MAX)),
            MAX_SUB_AGENT_TIMEOUT_SECS
        );
        assert_eq!(
            resolve_timeout_secs(Some(u64::MAX), None),
            MAX_SUB_AGENT_TIMEOUT_SECS
        );
    }

    #[test]
    fn a_revival_rebuilds_from_the_delegation_that_created_it_only() {
        // The batch envelope must not travel into the registry: a follow-up to
        // one delegate that re-ran all three would be a spend bug with no
        // symptom the model could see.
        let args = serde_json::json!({
            "task": "survey the repo",
            "tool_set": "read_only",
            "tasks": [{ "task": "something else" }],
            "persona": null
        });
        let stored = spawn_args(&args);
        assert_eq!(
            stored,
            serde_json::json!({ "task": "survey the repo", "tool_set": "read_only" })
        );
    }

    #[test]
    fn an_ad_hoc_delegate_gets_the_tools_its_tool_set_names() {
        assert_eq!(
            ad_hoc_tool_names("read_only", None),
            vec!["read_file", "list_files", "grep", "find_files"]
        );
        let full = ad_hoc_tool_names("full", None);
        assert!(full.contains(&"write_file".to_string()));
        assert!(full.contains(&"bash".to_string()));
    }

    /// The contract is the tool, so the prompt must not still be teaching the
    /// fence protocol it replaced — two contracts is how a model ends up
    /// satisfying the one nothing reads.
    #[test]
    fn the_prompt_teaches_one_contract() {
        assert!(SUB_AGENT_PROMPT_SUFFIX.contains(YIELD_TOOL_NAME));
        assert!(!SUB_AGENT_PROMPT_SUFFIX.contains("handoff"));
        assert!(!SUB_AGENT_PROMPT_SUFFIX.contains("```"));
    }

    fn plan_for_tests() -> ChildPlan {
        ChildPlan {
            label: "research-agent".into(),
            prompt: "p".into(),
            timeout: Duration::from_secs(10),
            tool_names: vec![],
            nested: None,
        }
    }

    /// The escalation is bounded, and its last rung is real: on the final
    /// attempt the child's registry holds nothing but the terminal tool, so
    /// with `ToolChoice::Required` already forbidding free text there is
    /// exactly one legal move left.
    #[test]
    fn the_last_attempt_withdraws_every_other_tool() {
        assert_eq!(MAX_YIELD_ATTEMPTS, 3);
        let tool = SubAgentTool::new("k".into(), "m".into(), "p".into());
        let plan = ChildPlan {
            tool_names: vec!["read_file".into(), "grep".into()],
            ..plan_for_tests()
        };
        let slot = crate::tools::yield_result::slot();

        let ordinary = tool.child_registry(&plan, slot.clone(), false);
        assert!(ordinary.names().contains(&"read_file"));
        assert!(
            ordinary.names().contains(&YIELD_TOOL_NAME),
            "the contract is always on the table, not only at the end"
        );

        let forced = tool.child_registry(&plan, slot, true);
        assert_eq!(forced.names(), vec![YIELD_TOOL_NAME]);
    }

    /// (a) A child that never meets the contract must not be silently accepted.
    #[test]
    fn a_child_that_never_yields_is_marked_unreconciled() {
        let _exclusive = delegates::exclusive();
        crate::tools::sub_agent_registry::reset();
        let tool = SubAgentTool::new("k".into(), "m".into(), "p".into())
            .with_instance(Some("inst".into()))
            .with_turn_plan(Some(crate::turn_plan::new_shared()));
        let plan = plan_for_tests();
        let id = delegates::open("inst", &plan.label, "survey", serde_json::json!({}));

        // What run_child produces after MAX_YIELD_ATTEMPTS of prose.
        let outcome = ChildOutcome::Settled {
            report: HandoffReport {
                completed: "I had a look and it seems fine.".into(),
                not_done: vec![],
                suggest_persona: None,
            },
            unreconciled: true,
            messages: vec![],
            transcript: "→ read_file\n← ok".into(),
            tools_used: vec!["read_file".into()],
            turns: 2,
        };
        let out = tool.settle_outcome(&id, &plan, outcome);

        assert_eq!(out["unreconciled"], true, "{out}");
        assert!(
            out["result_note"]
                .as_str()
                .unwrap()
                .contains("never called"),
            "the parent has to be able to tell a report from a guess: {out}"
        );
        assert_eq!(out["delegate_id"], id.as_str());
        // …and it is still filed, so the work is not lost with the contract.
        assert_eq!(
            delegates::list("inst")[0].state,
            delegates::DelegateState::Idle
        );
    }

    #[test]
    fn a_reported_handoff_names_the_delegate_the_parent_can_reach() {
        let _exclusive = delegates::exclusive();
        crate::tools::sub_agent_registry::reset();
        let plan_handle = crate::turn_plan::new_shared();
        let tool = SubAgentTool::new("k".into(), "m".into(), "p".into())
            .with_instance(Some("inst".into()))
            .with_turn_plan(Some(plan_handle.clone()));
        let plan = plan_for_tests();
        let id = delegates::open("inst", &plan.label, "survey", serde_json::json!({}));

        let out = tool.settle_outcome(
            &id,
            &plan,
            ChildOutcome::Settled {
                report: HandoffReport {
                    completed: "Found 4 stale claims in Hero.tsx.".into(),
                    not_done: vec!["edit Hero.tsx to drop the 4 stale claims".into()],
                    suggest_persona: Some("coding-agent".into()),
                },
                unreconciled: false,
                messages: vec![],
                transcript: "t".into(),
                tools_used: vec![],
                turns: 1,
            },
        );
        assert_eq!(out["completed"], false);
        assert_eq!(out["suggest_persona"], "coding-agent");

        let reason = crate::turn_plan::lock(&plan_handle)
            .blocking_reason()
            .expect("unfinished work holds the turn open");
        assert!(
            reason.contains(&id),
            "the gate must name something the model can address: {reason}"
        );
    }

    /// A killed delegate offers no revival; a suspended one does. Getting this
    /// backwards is the failure the two states exist to prevent.
    #[test]
    fn a_kill_is_terminal_and_a_suspension_is_not() {
        let _exclusive = delegates::exclusive();
        crate::tools::sub_agent_registry::reset();
        let tool =
            SubAgentTool::new("k".into(), "m".into(), "p".into()).with_instance(Some("inst".into()));
        let plan = plan_for_tests();

        let killed = delegates::open("inst", "slow-agent", "wait", serde_json::json!({}));
        let out = tool.settle_outcome(
            &killed,
            &plan,
            ChildOutcome::Killed {
                reason: "timed out after 120 seconds".into(),
                stopped: false,
            },
        );
        assert_eq!(out["delegate_state"], "aborted");
        assert!(out["result"].as_str().unwrap().contains("cannot be revived"));
        assert!(matches!(
            delegates::check_out("inst", &killed),
            Err(delegates::ReviveError::Aborted { .. })
        ));

        let parked = delegates::open("inst", "crashy-agent", "work", serde_json::json!({}));
        let out = tool.settle_outcome(
            &parked,
            &plan,
            ChildOutcome::Suspended {
                reason: "sub-agent failed at agent: 502".into(),
                transcript: "→ read_file\n← half of it".into(),
            },
        );
        assert_eq!(out["delegate_state"], "parked");
        assert!(matches!(
            delegates::check_out("inst", &parked),
            Ok(delegates::Revival::Reseed { .. })
        ));
    }

    /// (d) The parent's copy is bounded; the registry keeps the rest.
    #[test]
    fn a_huge_result_is_previewed_not_inlined() {
        let _exclusive = delegates::exclusive();
        crate::tools::sub_agent_registry::reset();
        let tool =
            SubAgentTool::new("k".into(), "m".into(), "p".into()).with_instance(Some("inst".into()));
        let plan = plan_for_tests();
        let id = delegates::open("inst", &plan.label, "survey", serde_json::json!({}));
        let huge = "finding after finding, at length. ".repeat(4_000);

        let out = tool.settle_outcome(
            &id,
            &plan,
            ChildOutcome::Settled {
                report: HandoffReport {
                    completed: huge.clone(),
                    not_done: vec![],
                    suggest_persona: None,
                },
                unreconciled: false,
                messages: vec![],
                transcript: huge.clone(),
                tools_used: vec![],
                turns: 1,
            },
        );

        let inline = out["result"].as_str().unwrap();
        assert!(
            inline.len() < huge.len() / 4,
            "an unbounded tool result is replayed into every later request: {}",
            inline.len()
        );
        assert_eq!(out["truncated"], true);
        assert_eq!(out["full_result_bytes"], huge.len());
        // The full text is still there, by id.
        let chunk = delegates::read("inst", &id, 0).expect("the transcript is retrievable");
        assert!(chunk.total_bytes > inline.len());
        assert!(chunk.text.contains("finding after finding"));
    }
}
