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
        "List all saved flows with id, name, and enabled state."
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
        "Read one flow by id, returning its full document (nodes, edges, schedule)."
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
        "Create or overwrite a flow. Provide a `flow` SavedFlow document; the `id` field on it identifies the flow. The flow is validated first — if it fails, nothing is saved and the errors are returned."
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

pub struct FlowInstallTool;

#[async_trait]
impl metalcraft::Tool for FlowInstallTool {
    fn name(&self) -> &str {
        "flow_install"
    }
    fn description(&self) -> &str {
        "Install a flow from the Metalcraft flows registry (flows.metalcraftai.com) by its slug. Downloads the flow, validates it, and saves it (disabled). Returns the installed flow plus a dependency report listing any packs/personas it still needs. Use flow_templates_list for built-in starting points; use this to pull a published flow from the registry."
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

pub struct FlowInstallDependenciesTool;

#[async_trait]
impl metalcraft::Tool for FlowInstallDependenciesTool {
    fn name(&self) -> &str {
        "flow_install_dependencies"
    }
    fn description(&self) -> &str {
        "Install and enable the integration packs a saved flow declares in its `requires` block. For each required pack: resolve its version range against the registry, download that exact version, verify the content hash, install, and enable it. Idempotent — packs already installed, enabled, and in-range are left untouched. Returns one outcome per pack (installed | already-satisfied | skipped | failed). Run this after flow_install when the dependency report lists missing packs, before flow_run."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Id of an already-installed flow" } },
            "required": ["id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = id_arg(&args, "flow_install_dependencies")?;
        let Some(flow) = metalcraft_flows::load_flow(&paths::flows_dir(), &id) else {
            return Ok(serde_json::json!({ "error": format!("flow '{id}' not found") }));
        };
        let outcomes = crate::flow_install::install_flow_dependencies(&flow).await;
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
        "Run a saved flow now (tools auto-approved), logged to a single flow-tagged diagnostics session. v2 flows run on the stateful state-machine executor and return `{ status, steps, variables }`; legacy v1 flows run every reachable prompt and return per-prompt results. Optionally set `persona` (default coding-agent), `model`, and `inputs` (a JSON object seeding the entry node's inputs for v2 flows)."
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
        let persona = args["persona"].as_str().unwrap_or("coding-agent");
        let model = args["model"].as_str().unwrap_or(DEFAULT_MODEL);
        let inputs: serde_json::Value = match args["inputs"].as_str() {
            Some(s) => serde_json::from_str(s)
                .unwrap_or_else(|_| serde_json::json!({})),
            None => serde_json::json!({}),
        };

        let context = match AgentRuntimeContext::from_environment() {
            Ok(c) => c,
            Err(e) => return Ok(serde_json::json!({ "error": format!("runtime not available: {e}") })),
        };
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());

        let Some(flow) = metalcraft_flows::load_flow(&paths::flows_dir(), &id) else {
            return Ok(serde_json::json!({ "error": format!("flow '{id}' not found") }));
        };

        if crate::flow_exec::is_v2_flow(&flow) {
            match crate::flow_exec::run_flow_v2(&context, flow, &cwd, persona, model, &inputs).await {
                Ok(summary) => Ok(serde_json::to_value(summary).unwrap_or(serde_json::Value::Null)),
                Err(e) => Ok(serde_json::json!({ "error": e })),
            }
        } else {
            match crate::flows::run_flow(&context, &id, &cwd, persona, model).await {
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
        let run_id = args["run_id"].as_str().ok_or_else(|| missing_param("flow_resume", "run_id"))?;
        let handle = args["handle"].as_str().ok_or_else(|| missing_param("flow_resume", "handle"))?;
        let data: Option<serde_json::Value> = args["data"]
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok());

        let context = match AgentRuntimeContext::from_environment() {
            Ok(c) => c,
            Err(e) => return Ok(serde_json::json!({ "error": format!("runtime not available: {e}") })),
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
        let run_id = args["run_id"].as_str().ok_or_else(|| missing_param("flow_run_status", "run_id"))?;
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
        let layered = crate::integration_packs::list_files_layered(
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
                let name = value.get("name").and_then(|v| v.as_str()).unwrap_or(&slug).to_string();
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
        let Some((path, _origin)) = crate::integration_packs::resolve_file(
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
