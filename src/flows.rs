use metalcraft_flows::{CoreNodeType, FlowNodeType, SavedFlow, ScheduledFlow, validate};
use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::Arc;

use serde::Serialize;

use crate::approval::ApprovalMode;
use crate::diagnostics::{DiagnosticsLogger, SessionInfo};
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

/// A schedule that is due to be considered, joined to the flow it runs.
///
/// The daemon works from these rather than from flows: a flow is only work, and
/// what the scheduler iterates is the list of things that are *going to happen*.
#[derive(Debug, Clone)]
pub struct RunnableSchedule {
    /// The scheduled flow, as stored. Its `id` keys the daemon's last-fired map,
    /// and its `instance_id` is the agent the run belongs to.
    pub scheduled: ScheduledFlow,
    /// The flow it runs.
    pub flow: SavedFlow,
    /// The parsed trigger.
    pub trigger: FlowSchedule,
}

/// Every enabled, timed schedule on this pod, joined to its flow and validated.
///
/// Skips — loudly — anything that cannot run: a schedule pointing at a flow that
/// no longer exists, a flow that fails validation, an unparseable cron. Loudly,
/// because the failure mode of a scheduler is silence, and "it just never ran" is
/// the hardest thing to debug about one.
pub fn load_due_candidates() -> Vec<RunnableSchedule> {
    let flows_dir = crate::paths::flows_dir();
    crate::scheduled_flows::list()
        .into_iter()
        .filter(ScheduledFlow::is_armed_timer)
        .filter_map(|scheduled| {
            let Some(flow) = metalcraft_flows::load_flow(&flows_dir, &scheduled.flow_id) else {
                log::warn!(
                    "Scheduled flow '{}' points at flow '{}', which does not exist",
                    scheduled.id,
                    scheduled.flow_id
                );
                return None;
            };
            match parse_schedule(&scheduled, &flow) {
                Ok(trigger) => Some(RunnableSchedule {
                    scheduled,
                    flow,
                    trigger,
                }),
                Err(err) => {
                    log::warn!("Skipping scheduled flow '{}': {err}", scheduled.id);
                    None
                }
            }
        })
        .collect()
}

/// Validate a scheduled flow against its flow and parse its trigger.
///
/// The graph is validated here too: a schedule is only as runnable as the flow it
/// points at, and finding out at 3am that the flow was malformed is worse than
/// finding out on the poll that skips it.
pub fn parse_schedule(scheduled: &ScheduledFlow, flow: &SavedFlow) -> Result<FlowSchedule, String> {
    let validation_errors = validate(flow);
    if !validation_errors.is_empty() {
        let joined = validation_errors
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("flow '{}' failed validation: {joined}", flow.id));
    }
    let schedule_errors = metalcraft_flows::validate_scheduled(scheduled);
    if !schedule_errors.is_empty() {
        return Err(schedule_errors
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; "));
    }

    match &scheduled.schedule.trigger {
        metalcraft_flows::ScheduleTrigger::Manual => Ok(FlowSchedule::Manual),
        metalcraft_flows::ScheduleTrigger::Minutes { interval } => {
            positive(*interval, &scheduled.id)?;
            Ok(FlowSchedule::EveryMinutes(*interval))
        }
        metalcraft_flows::ScheduleTrigger::Hours { interval } => {
            positive(*interval, &scheduled.id)?;
            Ok(FlowSchedule::EveryHours(*interval))
        }
        metalcraft_flows::ScheduleTrigger::Cron { cron } => {
            cron::Schedule::from_str(cron).map_err(|e| {
                format!(
                    "schedule '{}' has invalid cron expression '{cron}': {e}",
                    scheduled.id
                )
            })?;
            // A zone this pod cannot resolve is refused rather than fallen back
            // on. Falling back means the pod's own clock — UTC in the cluster —
            // so a mistyped zone fires at an hour nobody chose, and firing at
            // the wrong time is harder to notice than not firing at all. `save`
            // rejects these at the door; this catches what was written before.
            if let Some(zone) = scheduled.schedule.timezone.as_deref()
                && zone.parse::<chrono_tz::Tz>().is_err()
            {
                return Err(format!(
                    "schedule '{}' names timezone '{zone}', which this pod cannot \
                     resolve; use an IANA name like 'America/Detroit'",
                    scheduled.id
                ));
            }
            Ok(FlowSchedule::Cron(cron.clone()))
        }
    }
}

fn positive(interval: u64, schedule_id: &str) -> Result<(), String> {
    if interval == 0 {
        Err(format!(
            "schedule '{schedule_id}' has interval 0; expected > 0"
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
                    .ok_or_else(|| {
                        format!(
                            "flow '{}' prompt node '{}' is missing data.prompt",
                            flow.id, node.id
                        )
                    })?;
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
                    flow.id,
                    other.as_str(),
                    node.id
                ));
            }
            FlowNodeType::Custom(custom) => {
                return Err(format!(
                    "flow '{}' uses unsupported custom node type '{}' at '{}'",
                    flow.id, custom, node.id
                ));
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
                l.log_session_info(SessionInfo {
                    persona_name: &persona.name,
                    persona_slug,
                    model_name,
                    cwd,
                    system_prompt: &system_prompt,
                    tools: &persona.resolved_tool_names(),
                    skills: &persona.skills,
                    auto_approve: true,
                    flow_id: Some(flow_id),
                    // v1 flows predate agents and are never bound to one.
                    instance_id: None,
                });
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

    fn scheduled(json: &str) -> ScheduledFlow {
        serde_json::from_str(json).expect("parse scheduled flow")
    }

    /// A minimal, valid flow for the schedules below to point at.
    fn a_flow() -> SavedFlow {
        serde_json::from_str(
            r#"{
                "spec_version": "3", "id": "t", "name": "T",
                "created_at": "2026-05-28T00:00:00Z", "updated_at": "2026-05-28T00:00:00Z",
                "flow": { "nodes": [
                    { "id": "entry", "node_type": "entry", "data": {}, "position": [0,0] },
                    { "id": "p", "node_type": "prompt", "data": { "prompt": "hi" }, "position": [1,0] }
                ], "edges": [ { "id": "e", "source": "entry", "target": "p" } ] }
            }"#,
        )
        .expect("valid flow")
    }

    #[test]
    fn parse_schedule_reads_a_cron_trigger() {
        let sf = scheduled(
            r#"{ "id": "sf_1", "flow_id": "t", "created_at": "x", "updated_at": "x",
                 "schedule": { "type": "cron", "cron": "0 0 8 * * *", "timezone": "America/Detroit" } }"#,
        );
        let trigger = parse_schedule(&sf, &a_flow()).expect("valid");
        assert!(matches!(&trigger, FlowSchedule::Cron(c) if c == "0 0 8 * * *"));
        assert_eq!(sf.schedule.timezone.as_deref(), Some("America/Detroit"));
    }

    #[test]
    fn parse_schedule_rejects_bad_cron() {
        let sf = scheduled(
            r#"{ "id": "sf_1", "flow_id": "t", "created_at": "x", "updated_at": "x",
                 "schedule": { "type": "cron", "cron": "not a cron" } }"#,
        );
        let err = parse_schedule(&sf, &a_flow()).expect_err("unparseable cron");
        assert!(err.contains("invalid cron"), "{err}");
    }

    #[test]
    fn parse_schedule_rejects_a_zero_interval() {
        let sf = scheduled(
            r#"{ "id": "sf_1", "flow_id": "t", "created_at": "x", "updated_at": "x",
                 "schedule": { "type": "minutes", "interval": 0 } }"#,
        );
        assert!(parse_schedule(&sf, &a_flow()).is_err());
    }

    #[test]
    fn parse_schedule_rejects_a_schedule_whose_flow_is_invalid() {
        // A schedule is only as runnable as the flow it points at, and finding
        // that out on the poll that skips it beats finding out at 3am.
        let mut flow = a_flow();
        flow.flow.edges[0].target = "nonexistent".into();
        let sf = scheduled(
            r#"{ "id": "sf_1", "flow_id": "t", "created_at": "x", "updated_at": "x",
                 "schedule": { "type": "manual" } }"#,
        );
        let err = parse_schedule(&sf, &flow).expect_err("dangling edge");
        assert!(err.contains("failed validation"), "{err}");
    }

    #[test]
    fn only_enabled_timed_schedules_are_candidates() {
        // The daemon's filter, stated directly: `is_armed_timer` is the whole
        // rule, and there is no second switch anywhere for it to disagree with.
        let cases = [
            (r#"{"type":"cron","cron":"0 0 8 * * *"}"#, true, true),
            (r#"{"type":"cron","cron":"0 0 8 * * *"}"#, false, false),
            (r#"{"type":"manual"}"#, true, false),
            (r#"{"type":"manual"}"#, false, false),
        ];
        for (schedule, enabled, expected) in cases {
            let sf = scheduled(&format!(
                r#"{{ "id": "sf_1", "flow_id": "t", "enabled": {enabled},
                      "created_at": "x", "updated_at": "x", "schedule": {schedule} }}"#
            ));
            assert_eq!(
                sf.is_armed_timer(),
                expected,
                "schedule {schedule} enabled={enabled}"
            );
        }
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
        assert_eq!(
            prompts[0].persona.as_deref(),
            Some("discord-reporter-agent")
        );
    }

    #[test]
    fn node_persona_overrides_flow_persona() {
        let flow = flow_with_personas(Some("discord-reporter-agent"), Some("coding-agent"));
        let prompts = collect_reachable_prompts(&flow).unwrap();
        assert_eq!(prompts[0].persona.as_deref(), Some("coding-agent"));
    }
}
