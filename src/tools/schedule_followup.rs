//! `schedule_followup` — arm a deferred follow-up the daemon runs later.
//!
//! A chat turn is synchronous, so the agent can't block for minutes waiting to
//! re-check something. Instead it calls this tool to schedule a subagent task
//! for `delay`/`at` in the future, then ends its turn (typically with a
//! `say_to_user` "I'll check back in ~N min"). When the time comes, the daemon
//! runs the stored `task` as a subagent and delivers the result back to this
//! session's channel. See [`crate::scheduled_tasks`].

use async_trait::async_trait;
use chrono::Utc;

use crate::scheduled_tasks::{self, IoBinding, NewTask};

pub struct ScheduleFollowupTool {
    /// Where a fired follow-up should be delivered — captured from the current
    /// session. `None` outside a session (arms an unbound, log-only job).
    binding: Option<IoBinding>,
    /// Depth this session already carries; the armed job runs at depth + 1.
    reschedule_depth: u32,
}

impl ScheduleFollowupTool {
    pub fn new(binding: Option<IoBinding>, reschedule_depth: u32) -> Self {
        Self { binding, reschedule_depth }
    }
}

#[async_trait]
impl metalcraft::Tool for ScheduleFollowupTool {
    fn name(&self) -> &str {
        "schedule_followup"
    }

    fn description(&self) -> &str {
        "Schedule a follow-up to run LATER, then end your turn — use this instead \
         of claiming you'll 'check back', which you cannot do on your own. Give a \
         `delay` (e.g. \"3m\", \"90s\", \"2h\") OR an absolute `at` time, plus the \
         `task` to perform on wakeup (a self-contained instruction, e.g. 're-check \
         metalcraftai.com custom-domain status on Railway and report if HTTPS is \
         live'). When it fires, a sub-agent runs the task and its reply is \
         delivered back to this conversation. After scheduling, tell the user you'll \
         follow up. Optional `persona`/`tool_set`/`pack` scope the wakeup sub-agent \
         (same meaning as sub_agent); default runs it with read-only tools."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Self-contained instruction to run on wakeup. Include everything needed to act without this conversation's context (ids, domain names, what counts as done)."
                },
                "delay": {
                    "type": "string",
                    "description": "Relative time from now: \"90s\", \"3m\", \"2h\". Provide this OR `at`, not both."
                },
                "at": {
                    "type": "string",
                    "description": "Absolute RFC3339 time to run, e.g. \"2026-07-23T18:04:00Z\". Provide this OR `delay`."
                },
                "persona": {
                    "type": "string",
                    "description": "Run the wakeup sub-agent AS this persona (e.g. 'railway-agent') to inherit its tools/skills. Preferred for integration checks."
                },
                "tool_set": {
                    "type": "string",
                    "enum": ["read_only", "full", "all"],
                    "description": "Tool scope when no persona is given (same as sub_agent). Default 'read_only'."
                },
                "pack": {
                    "type": "string",
                    "description": "With tool_set='all', scope integration tools to one pack id (e.g. 'railway')."
                }
            },
            "required": ["task"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let task = args["task"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| crate::tools::missing_param(self.name(), "task"))?;

        let delay = args["delay"].as_str().filter(|s| !s.is_empty());
        let at = args["at"].as_str().filter(|s| !s.is_empty());

        let run_at = scheduled_tasks::resolve_run_at(delay, at, Utc::now())
            .map_err(|e| metalcraft::GraphError::ToolCallFailed {
                tool: self.name().into(),
                message: e,
            })?;

        let armed = scheduled_tasks::add(NewTask {
            io_binding: self.binding.clone().unwrap_or(IoBinding::Unbound),
            run_at,
            task: task.to_string(),
            persona: args["persona"].as_str().filter(|s| !s.is_empty()).map(String::from),
            tool_set: args["tool_set"].as_str().filter(|s| !s.is_empty()).map(String::from),
            pack: args["pack"].as_str().filter(|s| !s.is_empty()).map(String::from),
            reschedule_depth: self.reschedule_depth + 1,
        })
        .map_err(|e| metalcraft::GraphError::ToolCallFailed {
            tool: self.name().into(),
            message: e,
        })?;

        let deliverable = !matches!(armed.io_binding, IoBinding::Unbound);
        Ok(serde_json::json!({
            "scheduled_id": armed.id,
            "run_at": armed.run_at.to_rfc3339(),
            "delivered_here": deliverable,
            "note": if deliverable {
                "Scheduled. Tell the user you'll follow up when it runs."
            } else {
                "Scheduled, but this session has no delivery channel — the result will be logged only."
            }
        }))
    }
}
