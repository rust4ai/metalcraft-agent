//! Meta tools for **read-only** inspection of diagnostics sessions — the
//! workshop's "Chats"/diagnostics viewer, by prompt. Delegates to
//! `crate::diagnostics_browse` so the reconstruction matches the GUI. These
//! never mutate session data.

use async_trait::async_trait;

use crate::diagnostics_browse;
use crate::tools::missing_param;

pub struct DiagnosticsListTool;

#[async_trait]
impl metalcraft::Tool for DiagnosticsListTool {
    fn name(&self) -> &str {
        "diagnostics_list"
    }
    fn description(&self) -> &str {
        "List past agent/flow runs (diagnostics sessions), newest first, with id, persona, model, kind, and turn count."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }
    async fn call(&self, _args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        Ok(serde_json::json!({ "sessions": diagnostics_browse::list_diagnostics_sessions() }))
    }
}

pub struct DiagnosticsReadTool;

#[async_trait]
impl metalcraft::Tool for DiagnosticsReadTool {
    fn name(&self) -> &str {
        "diagnostics_read"
    }
    fn description(&self) -> &str {
        "Read one diagnostics session by id, returning its session_info and an ordered timeline of turns, LLM requests, compactions, errors, and config changes."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string", "description": "Session id (the timestamped directory name)" } },
            "required": ["id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| missing_param("diagnostics_read", "id"))?;
        match diagnostics_browse::read_diagnostics_session(id) {
            Some(session) => Ok(serde_json::to_value(session).unwrap_or(serde_json::Value::Null)),
            None => Ok(serde_json::json!({ "error": format!("diagnostics session '{id}' not found") })),
        }
    }
}
