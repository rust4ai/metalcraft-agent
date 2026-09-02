//! Meta tools for authoring and running **flows** by prompt — the workshop's
//! flow CRUD plus validate/run and flow-template browsing. Flow persistence and
//! the wire format come from the `metalcraft_flows` crate; execution reuses
//! `crate::flows::run_flow` so a prompt-driven run behaves exactly like the
//! workshop's run-flow endpoint.

use async_trait::async_trait;

use crate::paths;
use crate::runtime::{AgentRuntimeContext, DEFAULT_MODEL};
use crate::tools::missing_param;

fn id_arg(args: &serde_json::Value, tool: &str) -> metalcraft::Result<String> {
    args["id"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| missing_param(tool, "id"))
}

/// Parse the `flow` argument into a `SavedFlow`, returning a model-visible
/// error JSON on failure. Accepts either a JSON string (what the model sends —
/// object-typed tool params are rejected by strict function schemas) or an
/// inline object (convenient for direct/programmatic calls).
fn parse_flow(args: &serde_json::Value) -> Result<metalcraft_flows::SavedFlow, serde_json::Value> {
    let raw = args
        .get("flow")
        .ok_or_else(|| serde_json::json!({ "error": "Missing required parameter: flow" }))?;
    let value: serde_json::Value = if let Some(s) = raw.as_str() {
        serde_json::from_str(s)
            .map_err(|e| serde_json::json!({ "error": format!("invalid flow JSON: {e}") }))?
    } else {
        raw.clone()
    };
    serde_json::from_value(value)
        .map_err(|e| serde_json::json!({ "error": format!("invalid flow document: {e}") }))
}

fn validation_errors(flow: &metalcraft_flows::SavedFlow) -> Vec<String> {
    metalcraft_flows::validate(flow)
        .into_iter()
        .map(|e| e.to_string())
        .collect()
}

pub struct FlowListTool;

#[async_trait]
impl metalcraft::Tool for FlowListTool {
    fn name(&self) -> &str {
        "flow_list"
    }
    fn description(&self) -> &str {
        "List all saved flows with id and name. A flow carries no schedule and no enabled state — use scheduled_flow_list to see which of them run on a timer."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }
    async fn call(&self, _args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        Ok(serde_json::json!({ "flows": metalcraft_flows::list_flows(&paths::flows_dir()) }))
    }
}

pub struct FlowReadTool;

#[async_trait]
impl metalcraft::Tool for FlowReadTool {
    fn name(&self) -> &str {
        "flow_read"
    }
    fn description(&self) -> &str {
        "Read one flow by id, returning its full document (spec_version, nodes, edges). The document says only WHAT the work is — when it runs lives in a separate scheduled flow (scheduled_flow_list)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Flow id" } },
            "required": ["id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = id_arg(&args, "flow_read")?;
        match metalcraft_flows::load_flow(&paths::flows_dir(), &id) {
            Some(f) => Ok(serde_json::to_value(f).unwrap_or(serde_json::Value::Null)),
            None => Ok(serde_json::json!({ "error": format!("flow '{id}' not found") })),
        }
    }
}

pub struct FlowValidateTool;

#[async_trait]
impl metalcraft::Tool for FlowValidateTool {
    fn name(&self) -> &str {
        "flow_validate"
    }
    fn description(&self) -> &str {
        "Validate a flow document against the spec WITHOUT saving it. Returns `valid: true` or a list of errors. Call this before flow_write to catch problems early."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "flow": { "type": "string", "description": "A SavedFlow document as a JSON string" } },
            "required": ["flow"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let flow = match parse_flow(&args) {
            Ok(f) => f,
            Err(e) => return Ok(e),
        };
        let errors = validation_errors(&flow);
        // Advisory warnings (non-blocking): unhandled errors, dead nodes,
        // dangling variable references. Fix these even when `valid` is true.
        let warnings = crate::flow_exec::lint_flow(&flow);
        Ok(serde_json::json!({
            "valid": errors.is_empty(),
            "errors": errors,
            "warnings": warnings,
        }))
    }
}

pub struct FlowWriteTool;

#[async_trait]
impl metalcraft::Tool for FlowWriteTool {
    fn name(&self) -> &str {
        "flow_write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a flow — the WORK, not when it runs. Provide a `flow` SavedFlow document (spec_version \"3\"); the `id` field on it identifies the flow. Validated first: if it fails, nothing is saved and the errors are returned. Writing a flow schedules nothing — use scheduled_flow_create for that. Overwriting a flow leaves its existing schedules pointing at the new version."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "flow": { "type": "string", "description": "A SavedFlow document as a JSON string (must include `id`)" } },
            "required": ["flow"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let flow = match parse_flow(&args) {
            Ok(f) => f,
            Err(e) => return Ok(e),
        };
        let errors = validation_errors(&flow);
        if !errors.is_empty() {
            return Ok(serde_json::json!({ "saved": false, "errors": errors }));
        }
        match metalcraft_flows::save_flow(&paths::flows_dir(), &flow) {
            Ok(()) => Ok(serde_json::json!({ "saved": true, "id": flow.id })),
            Err(e) => Ok(serde_json::json!({ "error": e.to_string() })),
        }
    }
}

/// Shared: parse a `schedule` argument, which arrives either as a JSON string
/// (what strict function schemas force) or as an inline object.
fn schedule_arg(
    args: &serde_json::Value,
    tool: &str,
) -> Result<metalcraft_flows::ScheduleSpec, String> {
    let raw = args
        .get("schedule")
        .ok_or_else(|| format!("{tool}: missing `schedule`"))?;
    let value: serde_json::Value = match raw.as_str() {
        Some(s) => serde_json::from_str(s).map_err(|e| format!("invalid schedule JSON: {e}"))?,
        None => raw.clone(),
    };
    serde_json::from_value(value).map_err(|e| format!("invalid schedule spec: {e}"))
}

/// The shape every scheduled-flow tool describes to the model. Written once
/// because three tools take it and a drifting description is how a model learns a
/// field that does not exist.
const SCHEDULE_SHAPE: &str = "`{ \"type\": \"cron\"|\"minutes\"|\"hours\"|\"manual\", \"cron\"?: \"0 0 8 * * *\", \"interval\"?: number, \"name\"?: \"Morning brief\", \"timezone\"?: \"America/Detroit\", \"persona\"?: string, \"inputs\"?: object }`";

fn scheduled_view(sf: &metalcraft_flows::ScheduledFlow) -> serde_json::Value {
    let preview = crate::scheduled_flows::preview(&sf.schedule);
    serde_json::json!({
        "id": sf.id,
        "flow_id": sf.flow_id,
        "enabled": sf.enabled,
        "name": sf.schedule.display_name(),
        "description": preview.description,
        "next_runs": preview.next_runs,
        "instance_id": sf.instance_id,
        "schedule": sf.schedule,
    })
}

pub struct ScheduledFlowListTool;

#[async_trait]
impl metalcraft::Tool for ScheduledFlowListTool {
    fn name(&self) -> &str {
        "scheduled_flow_list"
    }
    fn description(&self) -> &str {
        "List everything this agent will do on its own: every scheduled flow, with its trigger, whether it is enabled, and when it fires next. Optionally filter by `flow_id`. An empty list means nothing runs unless somebody asks."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "flow_id": { "type": "string", "description": "Only schedules of this flow" }
            }
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let all = match args["flow_id"].as_str().filter(|s| !s.is_empty()) {
            Some(flow_id) => crate::scheduled_flows::for_flow(flow_id),
            None => crate::scheduled_flows::list(),
        };
        Ok(serde_json::json!({
            "scheduled": all.iter().map(scheduled_view).collect::<Vec<_>>()
        }))
    }
}

pub struct ScheduledFlowCreateTool;

#[async_trait]
impl metalcraft::Tool for ScheduledFlowCreateTool {
    fn name(&self) -> &str {
        "scheduled_flow_create"
    }
    fn description(&self) -> &str {
        "Schedule a flow: say WHEN an existing flow runs, and start running it. This also creates the persistent agent the schedule runs as, so successive firings remember each other — it is the \"yes, run this in the background\" step, and nothing runs on a timer without one. Schedules of the same flow share an agent by default. The `schedule` is a JSON object (or JSON string) of the form: see the `schedule` parameter."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "flow_id": { "type": "string", "description": "The flow to run" },
                "schedule": { "type": "string", "description": format!("The trigger, as a JSON object or JSON string: {SCHEDULE_SHAPE}") },
                "enabled": { "type": "boolean", "description": "Start firing immediately (default true)" },
                "instance_id": { "type": "string", "description": "Run as an existing agent instead of minting one" }
            },
            "required": ["flow_id", "schedule"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let flow_id = match args["flow_id"].as_str().filter(|s| !s.is_empty()) {
            Some(f) => f,
            None => return Err(missing_param("scheduled_flow_create", "flow_id")),
        };
        let schedule = match schedule_arg(&args, "scheduled_flow_create") {
            Ok(s) => s,
            Err(e) => return Ok(serde_json::json!({ "error": e })),
        };
        let Some(flow) = metalcraft_flows::load_flow(&paths::flows_dir(), flow_id) else {
            return Ok(serde_json::json!({ "error": format!("flow '{flow_id}' not found") }));
        };
        match crate::scheduled_flows::arm(crate::scheduled_flows::NewSchedule {
            flow: &flow,
            schedule,
            enabled: args["enabled"].as_bool().unwrap_or(true),
            instance: args["instance_id"].as_str().filter(|s| !s.is_empty()),
            from_suggestion: None,
            id: None,
        }) {
            Ok(sf) => Ok(serde_json::json!({ "created": true, "scheduled": scheduled_view(&sf) })),
            Err(e) => Ok(serde_json::json!({ "created": false, "error": e })),
        }
    }
}

pub struct ScheduledFlowUpdateTool;

#[async_trait]
impl metalcraft::Tool for ScheduledFlowUpdateTool {
    fn name(&self) -> &str {
        "scheduled_flow_update"
    }
    fn description(&self) -> &str {
        "Change an existing schedule: a new trigger, or pause/resume it with `enabled`. Pausing keeps the agent and everything it has learned. Use `scheduled_flow_list` first to get the id."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Scheduled flow id (from scheduled_flow_list)" },
                "schedule": { "type": "string", "description": format!("Replacement trigger, as a JSON object or JSON string: {SCHEDULE_SHAPE}") },
                "enabled": { "type": "boolean", "description": "Pause (false) or resume (true)" }
            },
            "required": ["id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = id_arg(&args, "scheduled_flow_update")?;
        let Some(mut sf) = crate::scheduled_flows::get(&id) else {
            return Ok(serde_json::json!({ "error": format!("no scheduled flow '{id}'") }));
        };
        if args.get("schedule").is_some() {
            match schedule_arg(&args, "scheduled_flow_update") {
                Ok(schedule) => sf.schedule = schedule,
                Err(e) => return Ok(serde_json::json!({ "error": e })),
            }
        }
        if let Some(enabled) = args["enabled"].as_bool() {
            sf.enabled = enabled;
        }
        sf.updated_at = chrono::Utc::now().to_rfc3339();
        match crate::scheduled_flows::save(&sf) {
            Ok(()) => Ok(serde_json::json!({ "saved": true, "scheduled": scheduled_view(&sf) })),
            Err(e) => Ok(serde_json::json!({ "saved": false, "error": e })),
        }
    }
}

pub struct ScheduledFlowDeleteTool;

#[async_trait]
impl metalcraft::Tool for ScheduledFlowDeleteTool {
    fn name(&self) -> &str {
        "scheduled_flow_delete"
    }
    fn description(&self) -> &str {
        "Stop running a flow on a timer, by scheduled flow id. The agent and everything it remembers are KEPT — this is 'stop doing this', not 'forget what you learned'. The flow itself is untouched and can still be run by hand."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Scheduled flow id" } },
            "required": ["id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = id_arg(&args, "scheduled_flow_delete")?;
        match crate::scheduled_flows::disarm(&id) {
            Ok(()) => Ok(serde_json::json!({ "deleted": id, "agent_kept": true })),
            Err(e) => Ok(serde_json::json!({ "error": e })),
        }
    }
}

pub struct FlowInstallTool;

#[async_trait]
impl metalcraft::Tool for FlowInstallTool {
    fn name(&self) -> &str {
        "flow_install"
    }
    fn description(&self) -> &str {
        "Install a flow from the Metalcraft flows registry (flows.metalcraftai.com) by its slug. Downloads the flow, validates it, and saves it — saving schedules nothing, so it will not run until you scheduled_flow_create or flow_run it. Returns the installed flow plus a dependency report listing any packs/personas it still needs. Use flow_templates_list for built-in starting points; use this to pull a published flow from the registry."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "slug": { "type": "string", "description": "Registry slug of the flow to install (equals its id)" } },
            "required": ["slug"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let slug = match args["slug"].as_str() {
            Some(s) => s.to_string(),
            None => return Err(missing_param("flow_install", "slug")),
        };
        match crate::flow_install::install_flow_from_registry(&slug).await {
            Ok(result) => Ok(serde_json::to_value(result).unwrap_or(serde_json::Value::Null)),
            Err(e) => Ok(serde_json::json!({ "error": e })),
        }
    }
}

pub struct FlowCheckDependenciesTool;

#[async_trait]
impl metalcraft::Tool for FlowCheckDependenciesTool {
    fn name(&self) -> &str {
        "flow_check_dependencies"
    }
    fn description(&self) -> &str {
        "Check whether this pod has the integrations a saved flow declares in its `requires` block. Returns one outcome per pack (already-satisfied | unsatisfied | missing). Run it after flow_install and before flow_run. It reports; it does not install — an integration reaches a pod inside an agent pack the operator installs, so tell the user which pack is missing rather than trying to fetch it yourself."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Id of an already-installed flow" } },
            "required": ["id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = id_arg(&args, "flow_check_dependencies")?;
        let Some(flow) = metalcraft_flows::load_flow(&paths::flows_dir(), &id) else {
            return Ok(serde_json::json!({ "error": format!("flow '{id}' not found") }));
        };
        let outcomes = crate::flow_install::check_flow_dependencies(&flow);
        Ok(serde_json::json!({ "flow": id, "packs": outcomes }))
    }
}

pub struct FlowDeleteTool;

#[async_trait]
impl metalcraft::Tool for FlowDeleteTool {
    fn name(&self) -> &str {
        "flow_delete"
    }
    fn description(&self) -> &str {
        "Delete a flow by id."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Flow id to delete" } },
            "required": ["id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = id_arg(&args, "flow_delete")?;
        if metalcraft_flows::delete_flow(&paths::flows_dir(), &id) {
            Ok(serde_json::json!({ "deleted": id }))
        } else {
            Ok(serde_json::json!({ "error": format!("flow '{id}' not found") }))
        }
    }
}

pub struct FlowRunTool;

#[async_trait]
impl metalcraft::Tool for FlowRunTool {
    fn name(&self) -> &str {
        "flow_run"
    }
    fn description(&self) -> &str {
        "Run a saved flow now (tools auto-approved), logged to a single flow-tagged diagnostics session. v2/v3 flows run on the stateful state-machine executor and return `{ status, steps, variables }`; legacy v1 flows run every reachable prompt and return per-prompt results. Optionally set `persona` (default coding-agent), `model`, and `inputs` (a JSON object seeding the entry node's inputs for v2/v3 flows)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Flow id to run" },
                "persona": { "type": "string", "description": "Persona to run prompts as (default: coding-agent). A node may override this." },
                "model": { "type": "string", "description": "Model to use (default: the runtime default)" },
                "inputs": { "type": "string", "description": "Optional JSON object (as a string) seeding a v2 flow's entry inputs" }
            },
            "required": ["id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = id_arg(&args, "flow_run")?;
        // Optional override only: a v2 flow declares its own persona on the entry
        // node. v1 flows fall back to "coding-agent" below.
        let persona_override = args["persona"].as_str().filter(|s| !s.trim().is_empty());
        let model = args["model"].as_str().unwrap_or(DEFAULT_MODEL);
        let inputs: serde_json::Value = match args["inputs"].as_str() {
            Some(s) => serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({})),
            None => serde_json::json!({}),
        };

        let context = match AgentRuntimeContext::from_environment() {
            Ok(c) => c,
            Err(e) => {
                return Ok(serde_json::json!({ "error": format!("runtime not available: {e}") }));
            }
        };
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());

        let Some(flow) = metalcraft_flows::load_flow(&paths::flows_dir(), &id) else {
            return Ok(serde_json::json!({ "error": format!("flow '{id}' not found") }));
        };

        if crate::flow_exec::is_v2_flow(&flow) {
            match crate::flow_exec::run_flow_v2(
                &context,
                flow,
                &cwd,
                persona_override,
                model,
                &inputs,
            )
            .await
            {
                Ok(summary) => Ok(serde_json::to_value(summary).unwrap_or(serde_json::Value::Null)),
                Err(e) => Ok(serde_json::json!({ "error": e })),
            }
        } else {
            let persona = persona_override
                .map(str::to_string)
                .unwrap_or_else(crate::runtime::configured_default_persona);
            match crate::flows::run_flow(&context, &id, &cwd, &persona, model).await {
                Ok(results) => Ok(serde_json::json!({ "flow_id": id, "prompts": results })),
                Err(e) => Ok(serde_json::json!({ "error": e })),
            }
        }
    }
}

pub struct FlowResumeTool;

#[async_trait]
impl metalcraft::Tool for FlowResumeTool {
    fn name(&self) -> &str {
        "flow_resume"
    }
    fn description(&self) -> &str {
        "Resume a paused flow run. Provide the `run_id` and the `handle` to take — for an approval node the decision (e.g. \"approve\"/\"reject\"), for a wait node \"after\". Optional `data` (JSON string) becomes the resumed node's `_last` input. Returns the run summary (which may pause again)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "Paused run id" },
                "handle": { "type": "string", "description": "Handle to take (approval decision, or 'after')" },
                "data": { "type": "string", "description": "Optional JSON value to set as the resumed node's _last input" }
            },
            "required": ["run_id", "handle"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let run_id = args["run_id"]
            .as_str()
            .ok_or_else(|| missing_param("flow_resume", "run_id"))?;
        let handle = args["handle"]
            .as_str()
            .ok_or_else(|| missing_param("flow_resume", "handle"))?;
        let data: Option<serde_json::Value> = args["data"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok());

        let context = match AgentRuntimeContext::from_environment() {
            Ok(c) => c,
            Err(e) => {
                return Ok(serde_json::json!({ "error": format!("runtime not available: {e}") }));
            }
        };
        match crate::flow_exec::resume_flow(&context, run_id, handle, data).await {
            Ok(summary) => Ok(serde_json::to_value(summary).unwrap_or(serde_json::Value::Null)),
            Err(e) => Ok(serde_json::json!({ "error": e })),
        }
    }
}

pub struct FlowRunStatusTool;

#[async_trait]
impl metalcraft::Tool for FlowRunStatusTool {
    fn name(&self) -> &str {
        "flow_run_status"
    }
    fn description(&self) -> &str {
        "Get the status of a flow run by id: status (running/paused/completed/failed), the node it's paused at, pause details, current variables, and step trace."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "run_id": { "type": "string", "description": "Run id" } },
            "required": ["run_id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let run_id = args["run_id"]
            .as_str()
            .ok_or_else(|| missing_param("flow_run_status", "run_id"))?;
        match crate::flow_runs::load_run(&paths::runs_dir(), run_id) {
            Some(run) => Ok(serde_json::to_value(run).unwrap_or(serde_json::Value::Null)),
            None => Ok(serde_json::json!({ "error": format!("run '{run_id}' not found") })),
        }
    }
}

pub struct FlowRunsListTool;

#[async_trait]
impl metalcraft::Tool for FlowRunsListTool {
    fn name(&self) -> &str {
        "flow_runs_list"
    }
    fn description(&self) -> &str {
        "List persisted flow runs (paused and finished), newest activity first. Optionally filter by `flow_id`."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "flow_id": { "type": "string", "description": "Optional flow id filter" } },
            "required": []
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let filter = args["flow_id"].as_str();
        let mut runs = crate::flow_runs::list_runs(&paths::runs_dir());
        if let Some(f) = filter {
            runs.retain(|r| r.flow_id == f);
        }
        runs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let summaries: Vec<serde_json::Value> = runs
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "run_id": r.id,
                    "flow_id": r.flow_id,
                    "status": r.status,
                    "current_node_id": r.current_node_id,
                    "pause": r.pause,
                    "updated_at": r.updated_at,
                })
            })
            .collect();
        Ok(serde_json::json!({ "runs": summaries }))
    }
}

pub struct FlowTemplatesListTool;

#[async_trait]
impl metalcraft::Tool for FlowTemplatesListTool {
    fn name(&self) -> &str {
        "flow_templates_list"
    }
    fn description(&self) -> &str {
        "List available flow templates (starting points for new flows), local and from enabled packs, with slug and name."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }
    async fn call(&self, _args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let layered = crate::integrations::list_files_layered(
            &paths::flow_templates_dir(),
            "flow_templates",
            "json",
        );
        let templates: Vec<serde_json::Value> = layered
            .into_iter()
            .filter_map(|(path, origin)| {
                let slug = path.file_stem()?.to_str()?.to_string();
                let content = std::fs::read_to_string(&path).ok()?;
                let value: serde_json::Value = serde_json::from_str(&content).ok()?;
                let name = value
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&slug)
                    .to_string();
                Some(serde_json::json!({
                    "slug": slug,
                    "name": name,
                    "pack_id": origin.pack_id(),
                }))
            })
            .collect();
        Ok(serde_json::json!({ "templates": templates }))
    }
}

pub struct FlowTemplateReadTool;

#[async_trait]
impl metalcraft::Tool for FlowTemplateReadTool {
    fn name(&self) -> &str {
        "flow_template_read"
    }
    fn description(&self) -> &str {
        "Read a flow template by slug, returning its flow document so it can be customized and saved with flow_write."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "slug": { "type": "string", "description": "Flow template slug" } },
            "required": ["slug"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let slug = args["slug"]
            .as_str()
            .ok_or_else(|| missing_param("flow_template_read", "slug"))?;
        let Some((path, _origin)) = crate::integrations::resolve_file(
            &paths::flow_templates_dir(),
            "flow_templates",
            &format!("{slug}.json"),
        ) else {
            return Ok(serde_json::json!({ "error": format!("template '{slug}' not found") }));
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(v) => Ok(serde_json::json!({ "slug": slug, "flow": v })),
                Err(e) => Ok(serde_json::json!({ "error": format!("parse error: {e}") })),
            },
            Err(e) => Ok(serde_json::json!({ "error": format!("read error: {e}") })),
        }
    }
}
