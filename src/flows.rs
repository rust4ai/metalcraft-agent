use metalcraft_flows::{validate, CoreNodeType, FlowNodeType, SavedFlow};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub enum FlowSchedule {
    Manual,
    EveryMinutes(u64),
    EveryHours(u64),
    Cron(String),
}

#[derive(Debug, Clone)]
pub struct RunnableFlow {
    pub saved: SavedFlow,
    pub schedule: FlowSchedule,
}

pub fn default_flows_dir() -> PathBuf {
    let cwd_based = PathBuf::from("flows");
    if cwd_based.is_dir() {
        return cwd_based;
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let exe_based = parent.join("flows");
            if exe_based.is_dir() {
                return exe_based;
            }
        }
    }

    cwd_based
}

pub fn load_enabled_flows(dir: &Path) -> Vec<RunnableFlow> {
    let summaries = metalcraft_flows::list_flows(dir);
    summaries
        .into_iter()
        .filter(|summary| summary.enabled)
        .filter_map(|summary| metalcraft_flows::load_flow(dir, &summary.id))
        .filter_map(|saved| match parse_schedule(&saved) {
            Ok(schedule) => Some(RunnableFlow { saved, schedule }),
            Err(err) => {
                log::warn!("Skipping flow due to invalid schedule: {err}");
                None
            }
        })
        .collect()
}

pub fn parse_schedule(flow: &SavedFlow) -> Result<FlowSchedule, String> {
    let validation_errors = validate(flow);
    if !validation_errors.is_empty() {
        let joined = validation_errors
            .into_iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("flow '{}' failed validation: {joined}", flow.id));
    }

    let entry = entry_node(flow)?;
    let schedule_type = entry
        .data
        .get("schedule_type")
        .and_then(|v| v.as_str())
        .unwrap_or("manual");

    match schedule_type {
        "manual" => Ok(FlowSchedule::Manual),
        "minutes" => Ok(FlowSchedule::EveryMinutes(read_positive_interval(entry.data.get("interval"), flow)?)),
        "hours" => Ok(FlowSchedule::EveryHours(read_positive_interval(entry.data.get("interval"), flow)?)),
        "cron" => {
            let expr = entry
                .data
                .get("cron")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("flow '{}' is missing entry.data.cron", flow.id))?;
            cron::Schedule::from_str(expr)
                .map_err(|e| format!("flow '{}' has invalid cron expression '{}': {}", flow.id, expr, e))?;
            Ok(FlowSchedule::Cron(expr.to_string()))
        }
        other => Err(format!("flow '{}' has unsupported schedule_type '{other}'", flow.id)),
    }
}

pub fn collect_reachable_prompt_texts(flow: &SavedFlow) -> Result<Vec<String>, String> {
    let entry = entry_node(flow)?;

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
                prompts.push(prompt.to_string());
            }
            FlowNodeType::Core(CoreNodeType::Branch) => {
                return Err(format!("flow '{}' uses unsupported node type 'branch' at '{}'", flow.id, node.id));
            }
            FlowNodeType::Core(CoreNodeType::BranchTool) => {
                return Err(format!("flow '{}' uses unsupported node type 'branch_tool' at '{}'", flow.id, node.id));
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

fn read_positive_interval(value: Option<&serde_json::Value>, flow: &SavedFlow) -> Result<u64, String> {
    let interval = value
        .and_then(|v| v.as_u64())
        .ok_or_else(|| format!("flow '{}' is missing a positive numeric entry.data.interval", flow.id))?;

    if interval == 0 {
        Err(format!("flow '{}' has interval 0; expected > 0", flow.id))
    } else {
        Ok(interval)
    }
}
