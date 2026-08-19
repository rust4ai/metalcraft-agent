use metalcraft_flows::{validate, CoreNodeType, FlowNodeType, SavedFlow};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use serde::Serialize;

use crate::approval::ApprovalMode;
use crate::diagnostics::DiagnosticsLogger;
use crate::persona::Persona;
use crate::runtime::{self, AgentRuntimeContext, RunOneShotRequest};
use metalcraft::RunOutcome;

#[derive(Debug, Clone)]
pub enum FlowSchedule {
    Manual,
    EveryMinutes(u64),
    EveryHours(u64),
    Cron(String),
}

/// One resolved, *enabled* schedule of a flow — a single trigger plus the
/// overrides applied when it fires. A flow may yield several of these.
#[derive(Debug, Clone)]
pub struct ScheduledTrigger {
    /// The schedule's stable id within the flow (unique per flow).
    pub schedule_id: String,
    /// The parsed trigger.
    pub schedule: FlowSchedule,
    /// IANA timezone a `Cron` trigger is evaluated in. `None` = host local time.
    pub timezone: Option<String>,
    /// Inputs handed to `run_flow_v2` when this schedule fires.
    pub inputs: Option<serde_json::Value>,
    /// Persona override for runs from this schedule.
    pub persona: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunnableFlow {
    pub saved: SavedFlow,
    /// The flow's enabled, non-manual triggers. Empty when the flow has only
    /// manual/disabled schedules (it stays loadable for manual runs but never
    /// fires on its own).
    pub triggers: Vec<ScheduledTrigger>,
}

pub fn load_enabled_flows(dir: &Path) -> Vec<RunnableFlow> {
    let summaries = metalcraft_flows::list_flows(dir);
    summaries
        .into_iter()
        .filter(|summary| summary.enabled)
        .filter_map(|summary| metalcraft_flows::load_flow(dir, &summary.id))
        .filter_map(|saved| match parse_schedules(&saved) {
            Ok(triggers) => Some(RunnableFlow { saved, triggers }),
            Err(err) => {
                log::warn!("Skipping flow due to invalid schedule: {err}");
                None
            }
        })
        .collect()
}

/// Resolve a flow's enabled, non-manual schedules into runnable triggers.
///
/// Reads the flow-level `schedules` array, falling back to the legacy entry-node
/// `schedule_type` (see [`metalcraft_flows::SavedFlow::effective_schedules`]).
/// Disabled and `manual` schedules are dropped — they never fire on their own.
/// A single invalid schedule (bad cron / zero interval) fails the whole flow so
/// the daemon log makes the misconfiguration obvious.
pub fn parse_schedules(flow: &SavedFlow) -> Result<Vec<ScheduledTrigger>, String> {
    let validation_errors = validate(flow);
    if !validation_errors.is_empty() {
        let joined = validation_errors
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("flow '{}' failed validation: {joined}", flow.id));
    }

    let mut triggers = Vec::new();
    for spec in flow.effective_schedules() {
        if !spec.enabled {
            continue;
        }
        let schedule = match &spec.trigger {
            metalcraft_flows::ScheduleTrigger::Manual => continue,
            metalcraft_flows::ScheduleTrigger::Minutes { interval } => {
                positive(*interval, &spec.id, flow)?;
                FlowSchedule::EveryMinutes(*interval)
            }
            metalcraft_flows::ScheduleTrigger::Hours { interval } => {
                positive(*interval, &spec.id, flow)?;
                FlowSchedule::EveryHours(*interval)
            }
            metalcraft_flows::ScheduleTrigger::Cron { cron } => {
                cron::Schedule::from_str(cron).map_err(|e| {
                    format!(
                        "flow '{}' schedule '{}' has invalid cron expression '{}': {}",
                        flow.id, spec.id, cron, e
                    )
                })?;
                FlowSchedule::Cron(cron.clone())
            }
        };
        triggers.push(ScheduledTrigger {
            schedule_id: spec.id.clone(),
            schedule,
            timezone: spec.timezone.clone(),
            inputs: spec.inputs.clone(),
            persona: spec.persona.clone(),
        });
    }
    Ok(triggers)
}

fn positive(interval: u64, schedule_id: &str, flow: &SavedFlow) -> Result<(), String> {
    if interval == 0 {
        Err(format!(
            "flow '{}' schedule '{}' has interval 0; expected > 0",
            flow.id, schedule_id
        ))
    } else {
        Ok(())
    }
}

/// A reachable prompt node plus the persona it should run as.
#[derive(Debug, Clone)]
pub struct FlowPrompt {
    pub prompt: String,
    /// Resolved persona slug: the prompt node's `data.persona`, falling back to
    /// the flow-level (entry node) `data.persona`. `None` means the daemon
    /// should use its default (`--persona`).
    pub persona: Option<String>,
}

pub fn collect_reachable_prompts(flow: &SavedFlow) -> Result<Vec<FlowPrompt>, String> {
    let entry = entry_node(flow)?;

    // Flow-level default persona, optionally set on the entry node.
    let flow_persona = entry
        .data
        .get("persona")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let node_map: HashMap<&str, &metalcraft_flows::FlowNode> = flow
        .flow
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();

    let mut outgoing: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &flow.flow.edges {
        outgoing
            .entry(edge.source.as_str())
            .or_default()
            .push(edge.target.as_str());
    }

    let mut queue = VecDeque::from([entry.id.as_str()]);
    let mut visited = HashSet::new();
    let mut prompts = Vec::new();

    while let Some(node_id) = queue.pop_front() {
        if !visited.insert(node_id) {
            continue;
        }

        let node = node_map
            .get(node_id)
            .ok_or_else(|| format!("flow '{}' references unknown node '{node_id}'", flow.id))?;

        match &node.node_type {
            FlowNodeType::Core(CoreNodeType::Entry) => {}
            FlowNodeType::Core(CoreNodeType::Prompt) => {
                let prompt = node
                    .data
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("flow '{}' prompt node '{}' is missing data.prompt", flow.id, node.id))?;
                let persona = node
                    .data
                    .get("persona")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| flow_persona.clone());
                prompts.push(FlowPrompt {
                    prompt: prompt.to_string(),
                    persona,
                });
            }
            // The legacy linear runner only understands entry+prompt chains.
            // Every other node type (v2 conditional/branch/effectors, or the
            // deprecated branch_tool, or custom vendor nodes) is handled by the
            // v2 `flow_exec::FlowExecutor`, not here.
            FlowNodeType::Core(other) => {
                return Err(format!(
                    "flow '{}' uses node type '{}' at '{}' — run it with the v2 executor",
                    flow.id, other.as_str(), node.id
                ));
            }
            FlowNodeType::Custom(custom) => {
                return Err(format!("flow '{}' uses unsupported custom node type '{}' at '{}'", flow.id, custom, node.id));
            }
        }

        if let Some(next) = outgoing.get(node_id) {
            for target in next {
                queue.push_back(target);
            }
        }
    }

    Ok(prompts)
}

/// Result of running one prompt node of a flow.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct FlowPromptResult {
    pub prompt_index: usize,
    /// "completed" | "interrupted" | "failed".
    pub status: String,
    pub answer: Option<String>,
    pub error: Option<String>,
}

/// Execute every reachable prompt of a saved flow as one-shot tasks, logging
/// all turns into a single flow-tagged diagnostics session. Shared by the
/// workshop API's run-flow endpoint and the `flow_run` meta tool so both
/// behave identically. Each prompt node may override the persona; otherwise
/// the `persona_slug` argument is used. Tools are auto-approved (flow runs are
/// non-interactive).
pub async fn run_flow(
    context: &AgentRuntimeContext,
    flow_id: &str,
    cwd: &str,
    persona_slug: &str,
    model_name: &str,
) -> Result<Vec<FlowPromptResult>, String> {
    let flow = metalcraft_flows::load_flow(&crate::paths::flows_dir(), flow_id)
        .ok_or_else(|| format!("flow '{flow_id}' not found"))?;
    let prompts = collect_reachable_prompts(&flow).map_err(|e| format!("unrunnable flow: {e}"))?;

    // One session for the whole run so it shows up in the Sessions list, tagged
    // with the flow id (kind == "flow"); prompt boundaries are config-change events.
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
                    Some(flow_id),
                );
            }
            Some(Arc::new(l))
        }
        Err(e) => {
            eprintln!("flow run: failed to create session logger: {e}");
            None
        }
    };

    let mut results = Vec::with_capacity(prompts.len());
    for (i, fp) in prompts.iter().enumerate() {
        let effective_persona = fp.persona.as_deref().unwrap_or(persona_slug);
        if let Some(l) = &logger {
            l.log_config_change(
                "flow_prompt",
                serde_json::json!({
                    "index": i,
                    "persona": effective_persona,
                    "prompt": fp.prompt,
                }),
            );
        }
        let outcome = runtime::run_one_shot_task(
            context,
            RunOneShotRequest {
                persona_slug: effective_persona,
                cwd,
                model_name,
                task: &fp.prompt,
                approval_mode: ApprovalMode::AutoApprove,
                diagnostics: logger.clone(),
                // v1 flows predate presets and are never bound to an agent.
                instance_id: None,
                preset_personas: None,
            },
        )
        .await;
        results.push(match outcome {
            Ok(RunOutcome::Completed(s)) => FlowPromptResult {
                prompt_index: i,
                status: "completed".into(),
                answer: s.final_answer().map(String::from),
                error: None,
            },
            Ok(RunOutcome::Interrupted { reason, .. }) => FlowPromptResult {
                prompt_index: i,
                status: "interrupted".into(),
                answer: None,
                error: Some(reason),
            },
            Ok(RunOutcome::Failed { node, error, .. }) => FlowPromptResult {
                prompt_index: i,
                status: "failed".into(),
                answer: None,
                error: Some(format!("{node}: {error}")),
            },
            Err(e) => FlowPromptResult {
                prompt_index: i,
                status: "failed".into(),
                answer: None,
                error: Some(e.to_string()),
            },
        });
    }
    Ok(results)
}

fn entry_node(flow: &SavedFlow) -> Result<&metalcraft_flows::FlowNode, String> {
    let entries: Vec<&metalcraft_flows::FlowNode> = flow
        .flow
        .nodes
        .iter()
        .filter(|node| matches!(node.node_type, FlowNodeType::Core(CoreNodeType::Entry)))
        .collect();

    match entries.as_slice() {
        [entry] => Ok(*entry),
        [] => Err(format!("flow '{}' has no entry node", flow.id)),
        _ => Err(format!("flow '{}' has multiple entry nodes", flow.id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow_with_personas(flow_persona: Option<&str>, prompt_persona: Option<&str>) -> SavedFlow {
        let entry_persona = flow_persona
            .map(|p| format!(", \"persona\": \"{p}\""))
            .unwrap_or_default();
        let prompt_persona = prompt_persona
            .map(|p| format!(", \"persona\": \"{p}\""))
            .unwrap_or_default();
        let json = format!(
            r#"{{
                "spec_version": "1",
                "id": "t",
                "name": "T",
                "created_at": "2026-05-28T00:00:00Z",
                "updated_at": "2026-05-28T00:00:00Z",
                "enabled": true,
                "flow": {{
                    "nodes": [
                        {{ "id": "entry", "node_type": "entry", "data": {{ "schedule_type": "manual"{entry_persona} }}, "position": [0, 0] }},
                        {{ "id": "p", "node_type": "prompt", "data": {{ "prompt": "do it"{prompt_persona} }}, "position": [1, 0] }}
                    ],
                    "edges": [ {{ "id": "e", "source": "entry", "target": "p" }} ]
                }}
            }}"#
        );
        serde_json::from_str(&json).expect("valid flow json")
    }

    #[test]
    fn parse_schedules_yields_enabled_non_manual_triggers() {
        // Two crons + one disabled + one manual → only the two enabled crons run.
        let json = r#"{
            "spec_version": "2",
            "id": "t", "name": "T",
            "created_at": "2026-05-28T00:00:00Z", "updated_at": "2026-05-28T00:00:00Z",
            "enabled": true,
            "schedules": [
                { "id": "morning", "type": "cron", "cron": "0 0 8 * * *", "timezone": "America/Detroit" },
                { "id": "evening", "type": "cron", "cron": "0 0 18 * * *" },
                { "id": "off", "type": "cron", "cron": "0 0 12 * * *", "enabled": false },
                { "id": "manual", "type": "manual" }
            ],
            "flow": { "nodes": [
                { "id": "entry", "node_type": "entry", "data": {}, "position": [0,0] },
                { "id": "p", "node_type": "prompt", "data": { "prompt": "hi" }, "position": [1,0] }
            ], "edges": [ { "id": "e", "source": "entry", "target": "p" } ] }
        }"#;
        let flow: SavedFlow = serde_json::from_str(json).unwrap();
        let triggers = parse_schedules(&flow).expect("valid schedules");
        assert_eq!(triggers.len(), 2);
        assert_eq!(triggers[0].schedule_id, "morning");
        assert_eq!(triggers[0].timezone.as_deref(), Some("America/Detroit"));
        assert!(matches!(triggers[1].schedule, FlowSchedule::Cron(_)));
    }

    #[test]
    fn parse_schedules_rejects_bad_cron() {
        let json = r#"{
            "spec_version": "2", "id": "t", "name": "T",
            "created_at": "2026-05-28T00:00:00Z", "updated_at": "2026-05-28T00:00:00Z",
            "enabled": true,
            "schedules": [ { "id": "bad", "type": "cron", "cron": "not a cron" } ],
            "flow": { "nodes": [ { "id": "entry", "node_type": "entry", "data": {}, "position": [0,0] } ], "edges": [] }
        }"#;
        let flow: SavedFlow = serde_json::from_str(json).unwrap();
        assert!(parse_schedules(&flow).is_err());
    }

    #[test]
    fn parse_schedules_falls_back_to_legacy_entry_cron() {
        let json = r#"{
            "spec_version": "1", "id": "t", "name": "T",
            "created_at": "2026-05-28T00:00:00Z", "updated_at": "2026-05-28T00:00:00Z",
            "enabled": true,
            "flow": { "nodes": [
                { "id": "entry", "node_type": "entry", "data": { "schedule_type": "cron", "cron": "0 0 9 * * *" }, "position": [0,0] }
            ], "edges": [] }
        }"#;
        let flow: SavedFlow = serde_json::from_str(json).unwrap();
        let triggers = parse_schedules(&flow).expect("valid");
        assert_eq!(triggers.len(), 1);
        assert!(matches!(&triggers[0].schedule, FlowSchedule::Cron(c) if c == "0 0 9 * * *"));
    }

    #[test]
    fn prompt_persona_defaults_to_none() {
        let flow = flow_with_personas(None, None);
        let prompts = collect_reachable_prompts(&flow).unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].prompt, "do it");
        assert_eq!(prompts[0].persona, None);
    }

    #[test]
    fn prompt_inherits_flow_level_persona() {
        let flow = flow_with_personas(Some("discord-reporter-agent"), None);
        let prompts = collect_reachable_prompts(&flow).unwrap();
        assert_eq!(prompts[0].persona.as_deref(), Some("discord-reporter-agent"));
    }

    #[test]
    fn node_persona_overrides_flow_persona() {
        let flow = flow_with_personas(Some("discord-reporter-agent"), Some("coding-agent"));
        let prompts = collect_reachable_prompts(&flow).unwrap();
        assert_eq!(prompts[0].persona.as_deref(), Some("coding-agent"));
    }
}
