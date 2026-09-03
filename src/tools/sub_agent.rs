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
/// The handoff block is the part that matters. A sub-agent that did 40% of the
/// job and one that finished return the same shape — prose — so the orchestrator
/// reads both as done, answers, and the turn ends three steps short. The
/// sub-agent that just read the code is the best-placed party in the system to
/// say what is left, so it is asked to say it in a form the parent can act on
/// rather than have to infer.
///
/// It is a fenced JSON block rather than a schema on the tool because the
/// sub-agent is a whole nested agent whose output is free text; asking for one
/// well-known block at the end is the cheapest contract that survives a model
/// that ignores it. An absent or unparseable block reads as `completed: true`
/// (see [`split_handoff`]) — a delegation that says nothing is treated exactly
/// as it was before this existed, so nothing regresses into false alarms.
const SUB_AGENT_PROMPT_SUFFIX: &str = "\n\nYou are a sub-agent. Complete the given task efficiently and report your findings. \
Be concise in your final answer.\n\n\
You have NONE of the parent conversation — only the task above. If it is missing something \
you need, say so rather than guessing.\n\n\
End your reply with this block, always:\n\n\
```handoff\n\
{\"completed\": true, \"not_done\": [], \"suggest_persona\": null}\n\
```\n\n\
Set `completed` to false if ANY part of the task is still outstanding — including work you \
could not do because you lack the tools for it (a read-only delegate cannot edit files; a \
delegate without an integration's tools cannot call it). List each outstanding item in \
`not_done` as a concrete one-liner naming files or targets, and name the persona best suited \
to finish it in `suggest_persona` if you know one. Reporting honestly that work remains is \
worth far more than a tidy-looking answer; the orchestrator will delegate the rest.";

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

/// What a sub-agent reported about its own completeness.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HandoffReport {
    completed: bool,
    not_done: Vec<String>,
    suggest_persona: Option<String>,
}

/// Split a sub-agent's final answer into (prose, report).
///
/// Looks for the LAST ```handoff fence, because a sub-agent explaining the
/// protocol mid-answer — or quoting a nested delegate's block — would otherwise
/// have its example parsed as its own status. The block is stripped from the
/// prose either way, so the parent never re-reads JSON it has already turned
/// into fields.
///
/// Every failure mode returns `None`: no fence, an unterminated fence,
/// unparseable JSON. `None` means "said nothing", and the caller treats that as
/// completed — a model that ignores the instruction gets exactly the behaviour
/// it had before the protocol existed, rather than a false report of unfinished
/// work that would hold the turn open for nothing.
fn split_handoff(answer: &str) -> (String, Option<HandoffReport>) {
    const FENCE: &str = "```handoff";

    let Some(start) = answer.rfind(FENCE) else {
        return (answer.trim().to_string(), None);
    };
    let after = &answer[start + FENCE.len()..];
    let Some(end_rel) = after.find("```") else {
        // An unterminated fence is a truncated answer, not a report. Leave the
        // prose whole rather than silently swallowing the tail.
        return (answer.trim().to_string(), None);
    };

    let body = &after[..end_rel];
    let prose = format!("{}{}", &answer[..start], &after[end_rel + 3..]);
    let prose = prose.trim().to_string();

    let Ok(value) = serde_json::from_str::<serde_json::Value>(body.trim()) else {
        return (prose, None);
    };

    let not_done = value
        .get("not_done")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // `completed: false` with an empty `not_done` is a sub-agent that knows it
    // fell short but did not say how. Believe the flag — the orchestrator can
    // still see the prose — but there is nothing specific to hand off.
    let completed = value
        .get("completed")
        .and_then(|v| v.as_bool())
        .unwrap_or(not_done.is_empty());

    (
        prose,
        Some(HandoffReport {
            completed,
            not_done,
            suggest_persona: value
                .get("suggest_persona")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
        }),
    )
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
         each other."
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
    async fn run_one(
        &self,
        args: &serde_json::Value,
    ) -> metalcraft::Result<serde_json::Value> {
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
            })?;

        // Two ways to scope the sub-agent:
        //   1. `persona` — run AS a named persona: its resolved tools (incl.
        //      pack-scoped integration tools), its system prompt, its skills.
        //      Preferred for integration work (e.g. persona "linear-agent").
        //   2. otherwise `tool_set` (read_only/full/all) [+ `pack`], using the
        //      parent's system prompt.
        let persona_slug = args["persona"].as_str().filter(|s| !s.is_empty());

        // What to call this delegate when reporting an unfinished handoff back to
        // the parent. The persona slug when there is one; otherwise the tool set,
        // which is the only identity an ad-hoc delegation has.
        let delegate_label = persona_slug.map(String::from).unwrap_or_else(|| {
            format!(
                "tool_set:{}",
                args["tool_set"].as_str().unwrap_or("read_only")
            )
        });

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
                // A nested sub-agent must not widen its own reach — and `None`
                // here used to mean exactly that, because `None` is *unscoped*
                // (see the roster check above, which only runs when there is a
                // roster). A preset-restricted agent could therefore delegate to
                // a delegate that reached any persona on the pod. The roster
                // travels down instead.
                preset_personas: self.preset_personas.clone(),
                sub_agent_depth: self.depth + 1,
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
                // The plan belongs to the parent turn. A sub-agent must not be
                // able to satisfy it (by writing steps it never did) or inherit
                // obligations it cannot see, so delegation stops here and the
                // nested run answers on its own terms.
                turn_plan: None,
                // Same reasoning as the plan, and it matters more here: the
                // scratchpad is the goal's entire memory, and a delegate holds
                // only the fragment of the task it was handed. One that rewrote
                // the document would be summarising a goal it cannot see. It
                // reports back instead, and the tick — which can see all of it —
                // writes.
                goal_id: None,
            };
            let registry = crate::tools::create_registry_for_with_config(
                &persona.resolved_tool_names(),
                Some(&config),
            );
            let sub_prompt = format!("{base_prompt}{SUB_AGENT_PROMPT_SUFFIX}");
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
            let sub_prompt = format!("{}{SUB_AGENT_PROMPT_SUFFIX}", self.system_prompt);
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
                let raw = state.final_answer().unwrap_or("(no answer)").to_string();
                let tools_used = state.tools_called();
                let turn_count = state.turns().len();

                // Split the machine-readable tail off the prose, so the parent
                // reads a clean answer and a separate, actionable status.
                let (answer, handoff) = split_handoff(&raw);

                let mut out = serde_json::json!({
                    "result": answer,
                    "tools_used": tools_used,
                    "turns": turn_count,
                    "completed": handoff.as_ref().is_none_or(|h| h.completed),
                });
                if let Some(report) = handoff.filter(|h| !h.completed) {
                    out["not_done"] = serde_json::json!(report.not_done);
                    if let Some(next) = &report.suggest_persona {
                        out["suggest_persona"] = serde_json::json!(next);
                    }
                    // Record the obligation where the reply tool will see it. A
                    // delegation that reported unfinished work now holds the turn
                    // open until the orchestrator does something about it.
                    if !report.not_done.is_empty() {
                        if let Some(plan) = &self.turn_plan {
                            crate::turn_plan::lock(plan).record_handoff(
                                crate::turn_plan::Handoff {
                                    from: delegate_label.clone(),
                                    not_done: report.not_done.clone(),
                                    suggest_persona: report.suggest_persona.clone(),
                                },
                            );
                        }
                    }
                }
                Ok(out)
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
        // What the nested config is built from — the parent's roster, not None.
        let nested = tool.preset_personas.clone();
        assert!(nested.is_some(), "a nested delegate must stay scoped");
    }

    #[test]
    fn a_delegation_tree_has_a_bottom() {
        // Depth is what bounds the tree. Without it the only limit is that each
        // branch eventually times out, which bounds a branch, not the spend.
        assert_eq!(MAX_SUB_AGENT_DEPTH, 2);
        let at_limit = SubAgentTool::new("k".into(), "m".into(), "p".into())
            .with_depth(MAX_SUB_AGENT_DEPTH);
        assert!(at_limit.depth >= MAX_SUB_AGENT_DEPTH);
        let root = SubAgentTool::new("k".into(), "m".into(), "p".into());
        assert_eq!(root.depth, 0, "a turn a person started is the top");
    }

    #[test]
    fn a_batch_keeps_writers_out() {
        // Two delegates editing one workspace at the same time overwrite each
        // other and neither notices, so the batch path refuses writers outright
        // rather than trusting a prompt to keep them out.
        assert!(!entry_writes_workspace(&serde_json::json!({ "task": "read it" })));
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
        // exercised through the goal task-dispatch test, which does not need a
        // live model to reach the guards.)
    }

    #[test]
    fn a_batch_is_bounded() {
        // Each delegate spends its own tokens, so one tool call must not be
        // able to quietly cost five.
        assert_eq!(MAX_PARALLEL_DELEGATES, 3);
    }

    #[test]
    fn an_answer_with_no_block_is_treated_as_complete() {
        let (prose, report) = split_handoff("I read the file and it looks fine.");
        assert_eq!(prose, "I read the file and it looks fine.");
        assert!(report.is_none());
    }

    #[test]
    fn an_unfinished_delegation_is_parsed_and_stripped() {
        let answer = "The page claims 4 features the repo no longer has.\n\n\
```handoff\n\
{\"completed\": false, \"not_done\": [\"edit Hero.tsx to drop the 4 stale claims\"], \"suggest_persona\": \"coding-agent\"}\n\
```";
        let (prose, report) = split_handoff(answer);
        assert_eq!(prose, "The page claims 4 features the repo no longer has.");
        let report = report.expect("block should parse");
        assert!(!report.completed);
        assert_eq!(
            report.not_done,
            vec!["edit Hero.tsx to drop the 4 stale claims"]
        );
        assert_eq!(report.suggest_persona.as_deref(), Some("coding-agent"));
    }

    /// A sub-agent that quotes the protocol before filling it in must not have
    /// its example read as its status.
    #[test]
    fn the_last_block_wins() {
        let answer = "I was told to end with ```handoff\n{\"completed\": true}\n``` \
so here it is.\n```handoff\n{\"completed\": false, \"not_done\": [\"the edits\"]}\n```";
        let (_, report) = split_handoff(answer);
        let report = report.expect("block should parse");
        assert!(!report.completed);
        assert_eq!(report.not_done, vec!["the edits"]);
    }

    #[test]
    fn a_malformed_block_never_invents_unfinished_work() {
        let (prose, report) = split_handoff("done\n```handoff\nnot json at all\n```");
        assert_eq!(prose, "done");
        assert!(report.is_none(), "unparseable means silent, not unfinished");

        let (_, report) = split_handoff("done\n```handoff\n{\"completed\": false");
        assert!(
            report.is_none(),
            "an unterminated fence is truncation, not a report"
        );
    }

    /// `completed` omitted: infer it from whether anything was listed, so a model
    /// that reports only `not_done` still holds the turn open.
    #[test]
    fn completed_is_inferred_from_not_done_when_absent() {
        let (_, report) = split_handoff("x\n```handoff\n{\"not_done\": [\"the tests\"]}\n```");
        let report = report.expect("block should parse");
        assert!(!report.completed);

        let (_, report) = split_handoff("x\n```handoff\n{\"not_done\": []}\n```");
        assert!(report.expect("block should parse").completed);
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
}
