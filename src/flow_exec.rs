//! v2 stateful flow executor.
//!
//! Walks a [`metalcraft_flows`] graph one node at a time, threading a shared
//! `variables` state object and routing by output handle — the general-purpose
//! state machine described in `docs/FLOWS_V2_STATE_MACHINE_PLAN.md`. This
//! supersedes the linear, stateless `crate::flows::run_flow` for v2 flows.
//!
//! Phase 1 implements: `entry`, `prompt`, `set_variable`, `tool`, and the pure
//! `conditional` router. `branch` (LLM classifier), `http`, `sub_agent`,
//! `approval`, `wait`, and `foreach` return a "not yet implemented" error until
//! later phases.

use metalcraft_flows::{
    evaluate,
    next_by_handle,
    nodes::{
        ApprovalData, BranchData, ConditionalData, EntryData, HttpData, PromptData,
        SetVariableData, SubAgentData, ToolData, WaitData,
    },
    resolve_template, CoreNodeType, FlowNode, FlowNodeType, Operator, SavedFlow, Variables,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use metalcraft::{
    create_react_agent_with_options, AgentMessage, AgentOptions, AgentState, Executor, RunOutcome,
    Tool, ToolChoice,
};

use crate::flow_runs::{FlowRun, PauseInfo};
use rig::client::CompletionClient;
use rig::providers::openai;

use crate::approval::{self, ApprovalMode};
use crate::diagnostics::DiagnosticsLogger;
use crate::persona::Persona;
use crate::runtime::{self, AgentRuntimeContext, RunOneShotRequest};

/// Default cap on node visits, so a cyclic graph can't run forever.
const DEFAULT_STEP_BUDGET: u32 = 100;

/// How a node runner tells the executor what to do next.
enum Route {
    /// Follow an outgoing edge, optionally via a named `source_handle`. `None`
    /// (or an unmatched handle) falls back to the node's unlabeled edge.
    Handle(Option<String>),
    /// Terminate the run with this status.
    End(String),
    /// Suspend the run at this node (human approval or durable wait), persisting
    /// a checkpoint. Resumed later via [`resume_flow`].
    Pause(PauseSpec),
}

/// Details of a pause returned by an `approval` / `wait` runner.
struct PauseSpec {
    reason: String,
    resume_handles: Vec<String>,
    message: Option<String>,
    wake_at: Option<String>,
}

/// One node's contribution to the run trace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowStep {
    /// The node that ran.
    pub node_id: String,
    /// Its wire-format type.
    pub node_type: String,
    /// `advanced` | `routed:<handle>` | `completed` | `failed`.
    pub outcome: String,
    /// Optional human-readable detail (answer snippet, error, chosen handle).
    pub detail: Option<String>,
}

/// The result of running (or pausing) a flow.
#[derive(Debug, Clone, Serialize)]
pub struct FlowRunSummary {
    /// The run id (matches the `runs/{id}.json` record when the run paused).
    pub run_id: String,
    /// The flow that ran.
    pub flow_id: String,
    /// `completed` | `failed` | `paused`.
    pub status: String,
    /// Per-node trace, in execution order.
    pub steps: Vec<FlowStep>,
    /// Final (or checkpointed) state — the `variables` object.
    pub variables: Value,
}

/// Stepwise executor over a single flow run.
pub struct FlowExecutor<'a> {
    context: &'a AgentRuntimeContext,
    flow: SavedFlow,
    cwd: String,
    /// Flow-level default persona (node data may override it).
    default_persona: String,
    model_name: String,
    variables: Variables,
    logger: Option<Arc<DiagnosticsLogger>>,
    step_budget: u32,
    steps: Vec<FlowStep>,
    /// Tools injected into every `branch` node's registry, in addition to the
    /// node persona's own tools. Used by tests to supply a mock (e.g. a fake
    /// weather tool) without a real integration; empty in production.
    extra_tools: Vec<Arc<dyn Tool>>,
    /// Stable id for this run; the `runs/{id}.json` filename when it pauses.
    run_id: String,
    /// Preserved creation timestamp across pause/resume (set when resumed).
    created_at: Option<String>,
}

impl<'a> FlowExecutor<'a> {
    /// Build an executor for `flow`, seeding state from the entry node's declared
    /// `inputs` and the caller-supplied `args` object. Returns an error naming any
    /// missing required inputs.
    pub fn new(
        context: &'a AgentRuntimeContext,
        flow: SavedFlow,
        cwd: &str,
        default_persona: &str,
        model_name: &str,
        args: &Value,
        logger: Option<Arc<DiagnosticsLogger>>,
    ) -> Result<Self, String> {
        // Flow-level persona override lives on the entry node's data.persona.
        let entry = entry_node(&flow)?;
        let flow_persona = entry
            .data
            .get("persona")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        // Seed variables from typed inputs, if any.
        let variables = match serde_json::from_value::<EntryData>(entry.data.clone()) {
            Ok(EntryData { inputs: Some(inputs), .. }) => {
                let (vars, missing) = Variables::seed_from_inputs(&inputs, args);
                if !missing.is_empty() {
                    return Err(format!("missing required inputs: {}", missing.join(", ")));
                }
                vars
            }
            _ => Variables::from_value(if args.is_object() { args.clone() } else { Value::Object(Default::default()) }),
        };

        Ok(Self {
            context,
            flow,
            cwd: cwd.to_string(),
            default_persona: flow_persona.unwrap_or_else(|| default_persona.to_string()),
            model_name: model_name.to_string(),
            variables,
            logger,
            step_budget: DEFAULT_STEP_BUDGET,
            steps: Vec::new(),
            extra_tools: Vec::new(),
            run_id: uuid::Uuid::new_v4().to_string(),
            created_at: None,
        })
    }

    /// Rebuild an executor from a persisted, paused [`FlowRun`] so it can resume.
    fn resumed(
        context: &'a AgentRuntimeContext,
        flow: SavedFlow,
        run: &FlowRun,
        logger: Option<Arc<DiagnosticsLogger>>,
    ) -> Self {
        Self {
            context,
            flow,
            cwd: run.cwd.clone(),
            default_persona: run.persona.clone(),
            model_name: run.model.clone(),
            variables: Variables::from_value(run.variables.clone()),
            logger,
            step_budget: DEFAULT_STEP_BUDGET,
            steps: run.steps.clone(),
            extra_tools: Vec::new(),
            run_id: run.id.clone(),
            created_at: Some(run.created_at.clone()),
        }
    }

    /// Inject extra tools into every `branch` node's registry (for testing with a
    /// mock tool). Returns `self` for chaining.
    pub fn with_extra_tools(mut self, tools: Vec<Arc<dyn Tool>>) -> Self {
        self.extra_tools = tools;
        self
    }

    /// Run the flow from its entry node until it terminates, pauses, or the step
    /// budget is exhausted.
    pub async fn run(self) -> Result<FlowRunSummary, String> {
        let start = entry_node(&self.flow)?.id.clone();
        self.drive(start).await
    }

    /// The core loop: advance from `current` node to node until a terminal or
    /// pause outcome. Shared by [`Self::run`] and resume.
    async fn drive(mut self, mut current: String) -> Result<FlowRunSummary, String> {
        let mut visits = 0u32;

        loop {
            visits += 1;
            if visits > self.step_budget {
                self.mark_terminal("failed");
                return Err(format!(
                    "flow '{}' exceeded step budget ({})",
                    self.flow.id, self.step_budget
                ));
            }

            let node = self
                .flow
                .flow
                .nodes
                .iter()
                .find(|n| n.id == current)
                .cloned()
                .ok_or_else(|| format!("flow '{}' references unknown node '{current}'", self.flow.id))?;

            let node_type = node.node_type.as_wire().to_string();
            let route = self.run_node(&node).await;

            match route {
                Ok(Route::End(status)) => {
                    self.steps.push(FlowStep {
                        node_id: current.clone(),
                        node_type,
                        outcome: "completed".into(),
                        detail: None,
                    });
                    self.mark_terminal(&status);
                    return Ok(self.into_summary(status));
                }
                Ok(Route::Handle(handle)) => {
                    let next = next_by_handle(&self.flow.flow, &current, handle.as_deref());
                    self.steps.push(FlowStep {
                        node_id: current.clone(),
                        node_type,
                        outcome: match &handle {
                            Some(h) => format!("routed:{h}"),
                            None => "advanced".into(),
                        },
                        detail: None,
                    });
                    match next {
                        Some(n) => current = n,
                        None => {
                            self.mark_terminal("completed");
                            return Ok(self.into_summary("completed".into()));
                        }
                    }
                }
                Ok(Route::Pause(spec)) => {
                    self.steps.push(FlowStep {
                        node_id: current.clone(),
                        node_type,
                        outcome: format!("paused:{}", spec.reason),
                        detail: spec.message.clone(),
                    });
                    self.persist_paused(&current, &spec);
                    return Ok(self.into_summary("paused".into()));
                }
                Err(e) => {
                    self.steps.push(FlowStep {
                        node_id: current.clone(),
                        node_type,
                        outcome: "failed".into(),
                        detail: Some(e.clone()),
                    });
                    self.mark_terminal("failed");
                    return Ok(self.into_summary("failed".into()));
                }
            }
        }
    }

    fn into_summary(self, status: String) -> FlowRunSummary {
        FlowRunSummary {
            run_id: self.run_id,
            flow_id: self.flow.id,
            status,
            steps: self.steps,
            variables: self.variables.into_value(),
        }
    }

    /// Write (or overwrite) this run's `runs/{id}.json` in the paused state.
    fn persist_paused(&self, node_id: &str, spec: &PauseSpec) {
        let dir = crate::paths::runs_dir();
        let now = Utc::now().to_rfc3339();
        // Preserve the original created_at across pause/resume cycles.
        let created_at = self
            .created_at
            .clone()
            .or_else(|| crate::flow_runs::load_run(&dir, &self.run_id).map(|r| r.created_at))
            .unwrap_or_else(|| now.clone());
        let run = FlowRun {
            id: self.run_id.clone(),
            flow_id: self.flow.id.clone(),
            status: "paused".into(),
            current_node_id: node_id.to_string(),
            variables: self.variables.as_value().clone(),
            pause: Some(PauseInfo {
                reason: spec.reason.clone(),
                resume_handles: spec.resume_handles.clone(),
                message: spec.message.clone(),
                wake_at: spec.wake_at.clone(),
            }),
            persona: self.default_persona.clone(),
            model: self.model_name.clone(),
            cwd: self.cwd.clone(),
            steps: self.steps.clone(),
            created_at,
            updated_at: now,
        };
        if let Err(e) = crate::flow_runs::save_run(&dir, &run) {
            eprintln!("flow run: failed to persist paused run '{}': {e}", self.run_id);
        }
    }

    /// If this run has a persisted record (i.e. it paused at least once), update
    /// it to a terminal status so `flow_run_status` reflects completion.
    fn mark_terminal(&self, status: &str) {
        let dir = crate::paths::runs_dir();
        if let Some(mut run) = crate::flow_runs::load_run(&dir, &self.run_id) {
            run.status = status.to_string();
            run.pause = None;
            run.variables = self.variables.as_value().clone();
            run.steps = self.steps.clone();
            run.updated_at = Utc::now().to_rfc3339();
            let _ = crate::flow_runs::save_run(&dir, &run);
        }
    }

    /// Dispatch one node to its runner.
    async fn run_node(&mut self, node: &FlowNode) -> Result<Route, String> {
        match &node.node_type {
            FlowNodeType::Core(CoreNodeType::Entry) => Ok(Route::Handle(None)),
            FlowNodeType::Core(CoreNodeType::End) => Ok(Route::End("completed".into())),
            FlowNodeType::Core(CoreNodeType::SetVariable) => self.run_set_variable(node),
            FlowNodeType::Core(CoreNodeType::Conditional) => self.run_conditional(node),
            FlowNodeType::Core(CoreNodeType::Prompt) => self.run_prompt(node).await,
            FlowNodeType::Core(CoreNodeType::Tool) => self.run_tool(node).await,
            FlowNodeType::Core(CoreNodeType::Http) => self.run_http(node).await,
            FlowNodeType::Core(CoreNodeType::SubAgent) => self.run_sub_agent(node).await,
            FlowNodeType::Core(CoreNodeType::Branch) => self.run_branch(node).await,
            FlowNodeType::Core(CoreNodeType::Approval) => self.run_approval(node),
            FlowNodeType::Core(CoreNodeType::Wait) => self.run_wait(node),
            FlowNodeType::Core(other) => Err(format!(
                "node type '{}' is not implemented yet (node '{}')",
                other.as_str(),
                node.id
            )),
            FlowNodeType::Custom(custom) => {
                Err(format!("custom node type '{custom}' is not executable (node '{}')", node.id))
            }
        }
    }

    // --- pure runners --------------------------------------------------------

    fn run_set_variable(&mut self, node: &FlowNode) -> Result<Route, String> {
        let data: SetVariableData = parse_data(node)?;
        let value = if let Some(from) = &data.from {
            let path = if from.trim().is_empty() || from == "." {
                "_last".to_string()
            } else {
                format!("_last.{from}")
            };
            self.variables.get(&path).cloned().unwrap_or(Value::Null)
        } else {
            interpolate_value(data.value.as_ref().unwrap_or(&Value::Null), self.variables.as_value())
        };
        self.variables.set(&data.variable, value.clone());
        self.variables.set_last(value);
        Ok(Route::Handle(None))
    }

    fn run_conditional(&mut self, node: &FlowNode) -> Result<Route, String> {
        let data: ConditionalData = parse_data(node)?;
        for cond in &data.conditions {
            let op = Operator::from_wire(&cond.operator)
                .ok_or_else(|| format!("unknown operator '{}' in node '{}'", cond.operator, node.id))?;
            if evaluate(op, self.variables.get(&cond.variable), cond.value.as_ref()) {
                return Ok(Route::Handle(Some(cond.handle.clone())));
            }
        }
        Ok(Route::Handle(data.default_handle.clone()))
    }

    // --- pause runners (checkpoint + resume) --------------------------------

    fn run_approval(&mut self, node: &FlowNode) -> Result<Route, String> {
        let data: ApprovalData = parse_data(node)?;
        let message = resolve_template(&data.message, self.variables.as_value());
        let choices = data
            .choices
            .clone()
            .unwrap_or_else(|| vec!["approve".into(), "reject".into()]);
        Ok(Route::Pause(PauseSpec {
            reason: "approval".into(),
            resume_handles: choices,
            message: Some(message),
            wake_at: None,
        }))
    }

    fn run_wait(&mut self, node: &FlowNode) -> Result<Route, String> {
        let data: WaitData = parse_data(node)?;
        let wake_at = if let Some(until) = &data.until {
            until.clone()
        } else if let Some(dur) = &data.duration {
            let secs = parse_duration_secs(dur)
                .ok_or_else(|| format!("wait node '{}': invalid duration '{dur}'", node.id))?;
            (Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339()
        } else {
            return Err(format!("wait node '{}' needs `duration` or `until`", node.id));
        };
        Ok(Route::Pause(PauseSpec {
            reason: "wait".into(),
            resume_handles: vec!["after".into()],
            message: None,
            wake_at: Some(wake_at),
        }))
    }

    // --- runtime-backed runners ---------------------------------------------

    async fn run_prompt(&mut self, node: &FlowNode) -> Result<Route, String> {
        let data: PromptData = parse_data(node)?;
        let prompt = resolve_template(&data.prompt, self.variables.as_value());
        let persona = data.persona.as_deref().unwrap_or(&self.default_persona);
        let model = data.model.as_deref().unwrap_or(&self.model_name);

        let outcome = runtime::run_one_shot_task(
            self.context,
            RunOneShotRequest {
                persona_slug: persona,
                cwd: &self.cwd,
                model_name: model,
                task: &prompt,
                approval_mode: ApprovalMode::AutoApprove,
                diagnostics: self.logger.clone(),
            },
        )
        .await;

        match outcome {
            Ok(RunOutcome::Completed(state)) => {
                let answer = state.final_answer().unwrap_or("").to_string();
                // If a schema is declared, try to parse the answer as JSON.
                let value = if data.output_schema.is_some() {
                    serde_json::from_str::<Value>(&answer).unwrap_or(Value::String(answer.clone()))
                } else {
                    Value::String(answer.clone())
                };
                if let Some(var) = &data.output_var {
                    self.variables.set(var, value.clone());
                }
                self.variables.set_last(value);
                Ok(Route::Handle(Some("ok".into())))
            }
            Ok(RunOutcome::Interrupted { reason, .. }) => {
                self.variables.set_last(Value::String(reason.clone()));
                Ok(Route::Handle(Some("error".into())))
            }
            Ok(RunOutcome::Failed { node: n, error, .. }) => {
                self.variables.set_last(Value::String(format!("{n}: {error}")));
                Ok(Route::Handle(Some("error".into())))
            }
            Err(e) => {
                self.variables.set_last(Value::String(e.to_string()));
                Ok(Route::Handle(Some("error".into())))
            }
        }
    }

    async fn run_tool(&mut self, node: &FlowNode) -> Result<Route, String> {
        let data: ToolData = parse_data(node)?;
        let args = interpolate_value(
            data.args.as_ref().unwrap_or(&Value::Object(Default::default())),
            self.variables.as_value(),
        );
        let registry = crate::tools::create_registry_for(std::slice::from_ref(&data.tool_name));
        match registry.call(&data.tool_name, args).await {
            Ok(result) => {
                if let Some(var) = &data.output_var {
                    self.variables.set(var, result.clone());
                }
                self.variables.set_last(result);
                Ok(Route::Handle(Some("ok".into())))
            }
            Err(e) => {
                self.variables.set_last(Value::String(e.to_string()));
                Ok(Route::Handle(Some("error".into())))
            }
        }
    }

    async fn run_http(&mut self, node: &FlowNode) -> Result<Route, String> {
        let data: HttpData = parse_data(node)?;
        let url = resolve_template(&data.url, self.variables.as_value());
        // Only http/https; the codebase itself calls http://localhost gateways,
        // so private hosts are intentionally not blocked here.
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            self.variables
                .set_last(Value::String(format!("unsupported url scheme: {url}")));
            return Ok(Route::Handle(Some("error".into())));
        }
        let method = reqwest::Method::from_bytes(data.method.to_uppercase().as_bytes())
            .unwrap_or(reqwest::Method::GET);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("metalcraft-agent (flow http node)")
            .build()
            .map_err(|e| format!("http node '{}': client: {e}", node.id))?;

        let mut req = client.request(method, &url);
        if let Some(Value::Object(headers)) = &data.headers {
            for (k, v) in headers {
                if let Some(vs) = v.as_str() {
                    req = req.header(k, resolve_template(vs, self.variables.as_value()));
                }
            }
        }
        if let Some(body) = &data.body {
            req = req.json(&interpolate_value(body, self.variables.as_value()));
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                let body = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
                let result = json!({ "status": status, "body": body });
                if let Some(var) = &data.output_var {
                    self.variables.set(var, result.clone());
                }
                self.variables.set_last(result);
                if (200..300).contains(&status) {
                    Ok(Route::Handle(Some("ok".into())))
                } else {
                    Ok(Route::Handle(Some("error".into())))
                }
            }
            Err(e) => {
                self.variables.set_last(Value::String(e.to_string()));
                Ok(Route::Handle(Some("error".into())))
            }
        }
    }

    async fn run_sub_agent(&mut self, node: &FlowNode) -> Result<Route, String> {
        let data: SubAgentData = parse_data(node)?;
        let task = resolve_template(&data.task, self.variables.as_value());

        // Reuse the sub_agent tool: it builds a scoped child agent (by persona or
        // tool_set/pack) and returns its result.
        let tool = crate::tools::sub_agent::SubAgentTool::new(
            self.context.api_key.clone(),
            self.model_name.clone(),
            "You are a helpful assistant.".to_string(),
        );
        let mut call_args = json!({ "task": task });
        if let Some(p) = &data.persona {
            call_args["persona"] = json!(p);
        }
        if let Some(ts) = &data.tool_set {
            call_args["tool_set"] = json!(ts);
        }
        if let Some(pk) = &data.pack {
            call_args["pack"] = json!(pk);
        }

        match tool.call(call_args).await {
            Ok(result) => {
                let is_err = result.get("error").and_then(|v| v.as_bool()).unwrap_or(false);
                let answer = result.get("result").cloned().unwrap_or(Value::Null);
                if let Some(var) = &data.output_var {
                    self.variables.set(var, answer.clone());
                }
                self.variables.set_last(answer);
                Ok(Route::Handle(Some(if is_err { "error" } else { "ok" }.into())))
            }
            Err(e) => {
                self.variables.set_last(Value::String(e.to_string()));
                Ok(Route::Handle(Some("error".into())))
            }
        }
    }

    /// The LLM classifier. Presents each `output` as a tool (typed by its
    /// `schema`), forces tool-only output, and terminates when the model calls
    /// exactly one of them. The chosen handle routes the edge; the tool call's
    /// arguments become the edge payload (`_last`, and the output's `var`).
    async fn run_branch(&mut self, node: &FlowNode) -> Result<Route, String> {
        let data: BranchData = parse_data(node)?;
        if data.outputs.is_empty() {
            return Err(format!("branch node '{}' declares no outputs", node.id));
        }
        let query = resolve_template(&data.query, self.variables.as_value());
        let model_name = data.model.clone().unwrap_or_else(|| self.model_name.clone());

        // System prompt + persona tools (or a minimal default with no persona).
        let (system_prompt, persona_tools) = match data.persona.as_deref() {
            Some(slug) => {
                let persona = Persona::load(slug, &self.context.personas_dir).map_err(|e| {
                    format!("branch node '{}': failed to load persona '{slug}': {e}", node.id)
                })?;
                (
                    persona.build_system_prompt(&self.context.skills_dir, &self.cwd),
                    persona.resolved_tool_names(),
                )
            }
            None => (
                "You are a decision step in a workflow. Use the available tools to \
                 gather any information you need, then call exactly one of the output \
                 tools to record your result. Never answer in free text."
                    .to_string(),
                Vec::new(),
            ),
        };

        // Registry = persona tools + injected extras + one HandleTool per output.
        let tool_config = crate::tools::ToolConfig {
            api_key: self.context.api_key.clone(),
            model_name: model_name.clone(),
            system_prompt: system_prompt.clone(),
            skills_dir: self.context.skills_dir.clone(),
            available_skills: Vec::new(),
            reply_sink: None,
            session_binding: None,
            reschedule_depth: 0,
        };
        let mut registry =
            crate::tools::create_registry_for_with_config(&persona_tools, Some(&tool_config));
        for t in &self.extra_tools {
            registry = registry.register(SharedTool(t.clone()));
        }
        let mut wrapped: HashMap<String, bool> = HashMap::new();
        let handle_names: Vec<String> = data.outputs.iter().map(|o| o.handle.clone()).collect();
        for out in &data.outputs {
            let (schema, is_wrapped) = adapt_schema(out.schema.as_ref());
            wrapped.insert(out.handle.clone(), is_wrapped);
            registry = registry.register(HandleTool {
                handle: out.handle.clone(),
                description: out
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("Select the '{}' outcome", out.handle)),
                schema,
            });
        }

        // A tool-only agent that must terminate by choosing exactly one handle.
        let client = openai::Client::new(&self.context.api_key)
            .map_err(|e| format!("branch node '{}': openai client: {e}", node.id))?;
        let model = client.completion_model(&model_name);
        let hook = approval::build_hook(ApprovalMode::AutoApprove);
        let graph = create_react_agent_with_options(
            model,
            registry,
            &system_prompt,
            AgentOptions {
                before_tool_call: hook,
                llm_call_hook: None,
                llm_response_hook: None,
                tool_choice: ToolChoice::Required,
                terminal_tools: handle_names.clone(),
            },
        )
        .map_err(|e| format!("branch node '{}': build agent: {e}", node.id))?
        .into_arc();

        let executor = Executor::new_from_arc(graph).max_steps(30);
        let outcome = executor.run(AgentState::new(query), "flow-branch").await;

        let chosen = match outcome {
            Ok(RunOutcome::Completed(state)) => find_last_handle_call(&state, &handle_names),
            _ => None,
        };

        match chosen {
            Some((handle, args)) => {
                let payload = if wrapped.get(&handle).copied().unwrap_or(false) {
                    args.get("value").cloned().unwrap_or(Value::Null)
                } else {
                    args
                };
                if let Some(out) = data.outputs.iter().find(|o| o.handle == handle)
                    && let Some(var) = &out.var
                {
                    self.variables.set(var, payload.clone());
                }
                self.variables.set_last(payload);
                Ok(Route::Handle(Some(handle)))
            }
            // No valid choice (timeout / model error) → fall back.
            None => Ok(Route::Handle(data.default_handle.clone())),
        }
    }
}

/// Adapt an output handle's declared `schema` into a function-parameters object
/// schema (LLM tool parameters MUST be an object). Returns `(schema, wrapped)`;
/// when `wrapped` is true the scalar payload lives under a `"value"` property and
/// must be unwrapped from the tool-call args.
fn adapt_schema(schema: Option<&Value>) -> (Value, bool) {
    match schema {
        Some(s) if s.get("type").and_then(|t| t.as_str()) == Some("object") => (s.clone(), false),
        Some(s) => (
            json!({ "type": "object", "properties": { "value": s }, "required": ["value"] }),
            true,
        ),
        None => (json!({ "type": "object", "properties": {} }), false),
    }
}

/// Find the most recent tool call whose name is one of `handles` (the terminal
/// handle selection), returning `(handle, args)`.
fn find_last_handle_call(state: &AgentState, handles: &[String]) -> Option<(String, Value)> {
    state.messages.iter().rev().find_map(|m| match m {
        AgentMessage::ToolCall { name, args, .. } if handles.iter().any(|h| h == name) => {
            Some((name.clone(), args.clone()))
        }
        _ => None,
    })
}

/// A synthetic tool representing one `branch` output handle. Its parameters are
/// the handle's schema; calling it just echoes the args (the executor reads the
/// selection + payload from the terminal tool call, not this return value).
struct HandleTool {
    handle: String,
    description: String,
    schema: Value,
}

#[async_trait]
impl Tool for HandleTool {
    fn name(&self) -> &str {
        &self.handle
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> Value {
        self.schema.clone()
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        Ok(args)
    }
}

/// Adapts an `Arc<dyn Tool>` back into a concrete `Tool` so it can be registered
/// (the registry's `register` takes a tool by value). Lets the executor inject
/// runtime-supplied tools into a branch registry.
struct SharedTool(Arc<dyn Tool>);

#[async_trait]
impl Tool for SharedTool {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn parameters_schema(&self) -> Value {
        self.0.parameters_schema()
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        self.0.call(args).await
    }
}

/// Whether a flow should run on the v2 [`FlowExecutor`] rather than the legacy
/// linear runner. True if it declares `spec_version = "2"` or uses any v2 core
/// node type — so existing v1 `entry`+`prompt` flows keep their exact
/// legacy semantics for back-compat.
pub fn is_v2_flow(flow: &SavedFlow) -> bool {
    flow.spec_version == "2"
        || flow
            .flow
            .nodes
            .iter()
            .any(|n| matches!(&n.node_type, FlowNodeType::Core(c) if c.is_v2()))
}

/// Validate `flow`, open a flow-tagged diagnostics session, and run it on the
/// executor. The v2 analog of [`crate::flows::run_flow`]: shared by the
/// `flow_run` tool, the daemon scheduler, and the workshop run-flow endpoint so
/// all three behave identically.
pub async fn run_flow_v2(
    context: &AgentRuntimeContext,
    flow: SavedFlow,
    cwd: &str,
    persona_slug: &str,
    model_name: &str,
    args: &Value,
) -> Result<FlowRunSummary, String> {
    let errors = metalcraft_flows::validate(&flow);
    if !errors.is_empty() {
        return Err(format!(
            "invalid flow: {}",
            errors.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ")
        ));
    }

    // One flow-tagged session so the run shows up in the Sessions list; the
    // executor's prompt/branch runners log their turns into it.
    let logger = match DiagnosticsLogger::new() {
        Ok(l) => {
            if let Ok(persona) = Persona::load(persona_slug, &context.personas_dir) {
                let system_prompt = persona.build_system_prompt(&context.skills_dir, cwd);
                l.log_session_info(
                    &persona.name,
                    persona_slug,
                    model_name,
                    cwd,
                    &system_prompt,
                    &persona.resolved_tool_names(),
                    &persona.skills,
                    true,
                    Some(&flow.id),
                );
            }
            Some(Arc::new(l))
        }
        Err(e) => {
            eprintln!("flow run: failed to create session logger: {e}");
            None
        }
    };

    let exec = FlowExecutor::new(context, flow, cwd, persona_slug, model_name, args, logger)?;
    exec.run().await
}

/// Resume a paused run by supplying the chosen handle — an `approval` decision,
/// or `"after"` for a `wait`. Optional `data` becomes the resumed node's `_last`
/// input. Loads the checkpoint from `runs/{run_id}.json`, routes away from the
/// pause node via `handle`, and drives to the next terminal or pause.
pub async fn resume_flow(
    context: &AgentRuntimeContext,
    run_id: &str,
    handle: &str,
    data: Option<Value>,
) -> Result<FlowRunSummary, String> {
    let dir = crate::paths::runs_dir();
    let run = crate::flow_runs::load_run(&dir, run_id)
        .ok_or_else(|| format!("run '{run_id}' not found"))?;
    if run.status != "paused" {
        return Err(format!("run '{run_id}' is '{}', not paused", run.status));
    }
    let flow = metalcraft_flows::load_flow(&crate::paths::flows_dir(), &run.flow_id)
        .ok_or_else(|| format!("flow '{}' not found", run.flow_id))?;

    let logger = match DiagnosticsLogger::new() {
        Ok(l) => {
            if let Ok(persona) = Persona::load(&run.persona, &context.personas_dir) {
                let system_prompt = persona.build_system_prompt(&context.skills_dir, &run.cwd);
                l.log_session_info(
                    &persona.name,
                    &run.persona,
                    &run.model,
                    &run.cwd,
                    &system_prompt,
                    &persona.resolved_tool_names(),
                    &persona.skills,
                    true,
                    Some(&run.flow_id),
                );
            }
            Some(Arc::new(l))
        }
        Err(_) => None,
    };

    let pause_node = run.current_node_id.clone();
    let mut exec = FlowExecutor::resumed(context, flow, &run, logger);
    if let Some(d) = data {
        exec.variables.set_last(d);
    }

    match next_by_handle(&exec.flow.flow, &pause_node, Some(handle)) {
        Some(next) => exec.drive(next).await,
        None => {
            exec.mark_terminal("completed");
            Ok(exec.into_summary("completed".into()))
        }
    }
}

/// Parse a simple duration (`"45s"`, `"30m"`, `"2h"`, `"1d"`) into seconds.
fn parse_duration_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.len().checked_sub(1)?);
    let n: i64 = num.trim().parse().ok()?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return None,
    };
    Some(n * mult)
}

/// Recursively resolve `{{…}}` placeholders in every string within a JSON value.
fn interpolate_value(v: &Value, vars: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(resolve_template(s, vars)),
        Value::Array(a) => Value::Array(a.iter().map(|x| interpolate_value(x, vars)).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, x)| (k.clone(), interpolate_value(x, vars))).collect())
        }
        other => other.clone(),
    }
}

/// Deserialize a node's `data` into its typed view, mapping errors to a
/// node-scoped message.
fn parse_data<T: serde::de::DeserializeOwned>(node: &FlowNode) -> Result<T, String> {
    serde_json::from_value(node.data.clone())
        .map_err(|e| format!("node '{}' has invalid data: {e}", node.id))
}

/// The single `entry` node of a flow.
fn entry_node(flow: &SavedFlow) -> Result<&FlowNode, String> {
    flow.flow
        .nodes
        .iter()
        .find(|n| matches!(n.node_type, FlowNodeType::Core(CoreNodeType::Entry)))
        .ok_or_else(|| format!("flow '{}' has no entry node", flow.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use metalcraft_flows::{FlowDefinition, FlowEdge};
    use serde_json::json;

    fn node(id: &str, ty: &str, data: Value) -> FlowNode {
        serde_json::from_value(json!({
            "id": id, "node_type": ty, "data": data, "position": [0, 0]
        }))
        .unwrap()
    }
    fn edge(id: &str, src: &str, tgt: &str, handle: Option<&str>) -> FlowEdge {
        FlowEdge {
            id: id.into(),
            source: src.into(),
            target: tgt.into(),
            source_handle: handle.map(str::to_string),
            target_handle: None,
        }
    }
    fn saved(nodes: Vec<FlowNode>, edges: Vec<FlowEdge>) -> SavedFlow {
        SavedFlow {
            spec_version: "2".into(),
            id: "t".into(),
            name: "T".into(),
            created_at: "2026-07-27T00:00:00Z".into(),
            updated_at: "2026-07-27T00:00:00Z".into(),
            enabled: false,
            flow: FlowDefinition { nodes, edges },
        }
    }

    /// Drive just the pure routing/state logic of the executor without a runtime
    /// (no `prompt`/`tool` nodes), so these run offline.
    async fn run_pure(flow: SavedFlow, args: Value) -> FlowRunSummary {
        // A context is only touched by runtime-backed runners; the pure runners
        // never read it, so a placeholder is fine for these tests.
        let ctx = AgentRuntimeContext {
            personas_dir: std::path::PathBuf::from("."),
            skills_dir: std::path::PathBuf::from("."),
            api_key: String::new(),
        };
        let exec = FlowExecutor::new(&ctx, flow, ".", "coding-agent", "test-model", &args, None)
            .expect("construct");
        exec.run().await.expect("run")
    }

    #[tokio::test]
    async fn set_variable_and_conditional_route_numerically() {
        // entry -> set_variable(_last := 18) -> conditional(_last > 50) -> hot/cold
        let flow = saved(
            vec![
                node("entry", "entry", json!({ "schedule_type": "manual" })),
                node("seed", "set_variable", json!({ "variable": "temp", "value": 18 })),
                node("check", "conditional", json!({
                    "conditions": [ { "handle": "hot", "variable": "_last", "operator": "gt", "value": 50 } ],
                    "default_handle": "cold"
                })),
                node("hot", "end", json!({ "status": "hot" })),
                node("cold", "end", json!({ "status": "cold" })),
            ],
            vec![
                edge("e0", "entry", "seed", None),
                edge("e1", "seed", "check", None),
                edge("e2", "check", "hot", Some("hot")),
                edge("e3", "check", "cold", Some("cold")),
            ],
        );
        let summary = run_pure(flow, json!({})).await;
        assert_eq!(summary.status, "completed");
        // 18 is not > 50, so it must route cold.
        let last = summary.steps.last().unwrap();
        assert_eq!(last.node_id, "cold");
        assert_eq!(summary.variables["temp"], json!(18));
    }

    #[tokio::test]
    async fn entry_inputs_seed_state_and_missing_required_errors() {
        let flow = saved(
            vec![
                node("entry", "entry", json!({
                    "schedule_type": "manual",
                    "inputs": { "city": { "type": "string", "required": true } }
                })),
                node("done", "end", json!({})),
            ],
            vec![edge("e0", "entry", "done", None)],
        );

        let summary = run_pure(flow.clone(), json!({ "city": "Madrid" })).await;
        assert_eq!(summary.status, "completed");
        assert_eq!(summary.variables["city"], json!("Madrid"));

        // Missing the required input fails construction.
        let ctx = AgentRuntimeContext {
            personas_dir: ".".into(),
            skills_dir: ".".into(),
            api_key: String::new(),
        };
        let err = match FlowExecutor::new(&ctx, flow, ".", "coding-agent", "m", &json!({}), None) {
            Ok(_) => panic!("expected missing-input error"),
            Err(e) => e,
        };
        assert!(err.contains("city"), "{err}");
    }

    #[test]
    fn v2_dispatch_detection() {
        // A legacy v1 entry+prompt flow stays on the legacy runner.
        let v1 = saved(
            vec![node("e", "entry", json!({})), node("p", "prompt", json!({ "prompt": "hi" }))],
            vec![edge("x", "e", "p", None)],
        );
        let mut v1 = v1;
        v1.spec_version = "1".into();
        assert!(!is_v2_flow(&v1));

        // A flow using a v2 node type routes to the executor even at "1"…
        let mut with_cond = saved(
            vec![
                node("e", "entry", json!({})),
                node("c", "conditional", json!({ "conditions": [] })),
            ],
            vec![edge("x", "e", "c", None)],
        );
        with_cond.spec_version = "1".into();
        assert!(is_v2_flow(&with_cond));

        // …and declaring spec_version "2" is enough on its own.
        let mut v2 = saved(vec![node("e", "entry", json!({}))], vec![]);
        v2.spec_version = "2".into();
        assert!(is_v2_flow(&v2));
    }

    #[tokio::test]
    async fn http_bad_scheme_routes_error() {
        // entry -> http(non-http url) -> error handle -> err end; ok -> ok end.
        let flow = saved(
            vec![
                node("entry", "entry", json!({ "schedule_type": "manual" })),
                node("call", "http", json!({ "method": "GET", "url": "ftp://nope/x" })),
                node("ok", "end", json!({ "status": "ok" })),
                node("err", "end", json!({ "status": "err" })),
            ],
            vec![
                edge("e0", "entry", "call", None),
                edge("e1", "call", "ok", Some("ok")),
                edge("e2", "call", "err", Some("error")),
            ],
        );
        let summary = run_pure(flow, json!({})).await;
        assert_eq!(summary.steps.last().unwrap().node_id, "err");
        assert!(
            summary.variables["_last"].as_str().unwrap_or("").contains("scheme"),
            "_last = {}",
            summary.variables["_last"]
        );
    }

    #[tokio::test]
    async fn set_variable_from_last_path() {
        // set_last to an object, then copy a nested field out with `from`.
        let flow = saved(
            vec![
                node("entry", "entry", json!({ "schedule_type": "manual" })),
                node("obj", "set_variable", json!({ "variable": "payload", "value": { "id": 7 } })),
                node("pick", "set_variable", json!({ "variable": "the_id", "from": "id" })),
                node("done", "end", json!({})),
            ],
            vec![
                edge("e0", "entry", "obj", None),
                edge("e1", "obj", "pick", None),
                edge("e2", "pick", "done", None),
            ],
        );
        let summary = run_pure(flow, json!({})).await;
        assert_eq!(summary.variables["the_id"], json!(7));
    }
}
