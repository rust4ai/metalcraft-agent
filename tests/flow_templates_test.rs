//! The flow templates this pod seeds are held to the spec they teach.
//!
//! A template is the first flow most people open and the shape they copy from —
//! `copyFlow` in the clients spreads the source document, so whatever a template
//! carries rides along into the flow someone starts from it. That makes a stale
//! template worse than a missing one: it is a wrong example that installs itself.
//!
//! The pod's own `/flows/validate` is not enough to catch that. It checks the
//! graph's integrity and the two node types whose routing depends on `data`
//! shape (`conditional`, `branch`), and it accepts spec v1 and v2 documents
//! forever, on purpose, so old flows keep loading. A template can therefore be
//! perfectly "valid" and still teach the format two versions ago — which is
//! exactly what happened to the pack templates that sat at v2 for a release
//! after v3 split scheduling out of the flow document.
//!
//! These rules are mirrored by `check_flow_template` in
//! `metalcraft-agent-external-packs/scripts/validate-packs.py`, which holds the
//! templates that ship inside packs to the same standard. Two copies because
//! that repo has no Rust; a rule added here belongs there too.

use metalcraft_flows::model::{CoreNodeType, FlowNodeType, SavedFlow};
use metalcraft_flows::nodes::{
    ApprovalData, BranchData, ConditionalData, EntryData, HttpData, PromptData, SetVariableData,
    SubAgentData, ToolData,
};
use serde_json::Value;
use std::collections::HashSet;

/// Variables the runtime seeds itself, so a template may read them without
/// declaring them.
const RUNTIME_VARS: [&str; 2] = ["_last", "_inputs"];

#[test]
fn every_seeded_flow_template_conforms_to_the_current_spec() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("seed/flow_templates");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("seed/flow_templates is missing")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no seeded templates found in {dir:?}");

    let mut problems = Vec::new();
    for path in &files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let raw = std::fs::read_to_string(path).expect("reading template");
        let doc: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                problems.push(format!("{name}: not JSON: {e}"));
                continue;
            }
        };
        let flow: SavedFlow = match serde_json::from_value(doc.clone()) {
            Ok(f) => f,
            Err(e) => {
                problems.push(format!("{name}: not a flow document: {e}"));
                continue;
            }
        };
        for problem in audit(&doc, &flow) {
            problems.push(format!("{name}: {problem}"));
        }
    }

    assert!(
        problems.is_empty(),
        "seeded flow templates are not conformant:\n  - {}",
        problems.join("\n  - ")
    );
}

/// Everything wrong with one template.
fn audit(doc: &Value, flow: &SavedFlow) -> Vec<String> {
    let mut problems: Vec<String> = metalcraft_flows::validate(flow)
        .iter()
        .map(ToString::to_string)
        .collect();

    if flow.spec_version != metalcraft_flows::SPEC_VERSION {
        problems.push(format!(
            "spec_version is {:?}; templates ship at the current spec ({:?})",
            flow.spec_version,
            metalcraft_flows::SPEC_VERSION
        ));
    }
    // v3 moved *when* a flow runs into a ScheduledFlow. A template still
    // carrying scheduling teaches a document shape that no longer exists.
    for legacy in ["enabled", "schedules"] {
        if doc.get(legacy).is_some() {
            problems.push(format!("carries `{legacy}`, removed in spec v3"));
        }
    }

    let entries: Vec<_> = flow
        .flow
        .nodes
        .iter()
        .filter(|n| matches!(n.node_type, FlowNodeType::Core(CoreNodeType::Entry)))
        .collect();
    if entries.len() != 1 {
        problems.push(format!(
            "has {} entry nodes; a runnable flow has exactly one",
            entries.len()
        ));
    }

    // Everything a later node may read: declared inputs, plus what each node
    // publishes. A `{{name}}` that resolves to nothing renders as the empty
    // string, so an undeclared reference is silent at runtime and only visible
    // as a prompt that reads oddly.
    let mut produced: HashSet<String> = RUNTIME_VARS.iter().map(|s| s.to_string()).collect();
    for entry in &entries {
        for key in ["schedule_type", "interval", "cron"] {
            if entry.data.get(key).is_some() {
                problems.push(format!("entry node carries legacy scheduling key `{key}`"));
            }
        }
        match serde_json::from_value::<EntryData>(entry.data.clone()) {
            Ok(data) => produced.extend(data.inputs.unwrap_or_default().into_keys()),
            Err(e) => problems.push(format!("entry node data: {e}")),
        }
    }

    for node in &flow.flow.nodes {
        let id = &node.id;
        // Parse each node's `data` as the type it declares. The spec validator
        // checks only `conditional` and `branch`; a `prompt` with no `prompt` or
        // an `http` with no `url` is accepted there and fails at execution.
        let data = node.data.clone();
        let typed = |r: Result<(), serde_json::Error>| r.err().map(|e| format!("node {id:?}: {e}"));
        let complaint = match node.node_type {
            FlowNodeType::Core(CoreNodeType::Prompt) => {
                typed(serde_json::from_value::<PromptData>(data).map(|_| ()))
            }
            FlowNodeType::Core(CoreNodeType::Tool) => {
                typed(serde_json::from_value::<ToolData>(data).map(|_| ()))
            }
            FlowNodeType::Core(CoreNodeType::Http) => {
                typed(serde_json::from_value::<HttpData>(data).map(|_| ()))
            }
            FlowNodeType::Core(CoreNodeType::Branch) => {
                typed(serde_json::from_value::<BranchData>(data).map(|_| ()))
            }
            FlowNodeType::Core(CoreNodeType::Conditional) => {
                typed(serde_json::from_value::<ConditionalData>(data).map(|_| ()))
            }
            FlowNodeType::Core(CoreNodeType::Approval) => {
                typed(serde_json::from_value::<ApprovalData>(data).map(|_| ()))
            }
            FlowNodeType::Core(CoreNodeType::SetVariable) => {
                typed(serde_json::from_value::<SetVariableData>(data).map(|_| ()))
            }
            FlowNodeType::Core(CoreNodeType::SubAgent) => {
                typed(serde_json::from_value::<SubAgentData>(data).map(|_| ()))
            }
            _ => None,
        };
        problems.extend(complaint);

        if let Some(var) = node.data.get("output_var").and_then(Value::as_str) {
            produced.insert(var.to_string());
        }
        if let Some(var) = node.data.get("item_var").and_then(Value::as_str) {
            produced.insert(var.to_string());
        }
        if matches!(node.node_type, FlowNodeType::Core(CoreNodeType::SetVariable))
            && let Some(var) = node.data.get("variable").and_then(Value::as_str)
        {
            produced.insert(var.to_string());
        }
        if let Some(outputs) = node.data.get("outputs").and_then(Value::as_array) {
            for var in outputs.iter().filter_map(|o| o.get("var")?.as_str()) {
                produced.insert(var.to_string());
            }
        }

        // A node that can fail routes `error`. An unlabeled edge is the fallback
        // for *every* handle, so wiring one and no `error` edge turns a failure
        // into a success that carries the error text forward as its result.
        let failable = matches!(
            node.node_type,
            FlowNodeType::Core(
                CoreNodeType::Prompt
                    | CoreNodeType::Tool
                    | CoreNodeType::Http
                    | CoreNodeType::SubAgent
                    | CoreNodeType::Branch
            )
        );
        if failable {
            let outgoing: Vec<_> = flow.flow.edges.iter().filter(|e| &e.source == id).collect();
            let unlabeled = outgoing
                .iter()
                .find(|e| matches!(e.source_handle.as_deref(), None | Some("default")));
            let has_error = outgoing
                .iter()
                .any(|e| e.source_handle.as_deref() == Some("error"));
            if let Some(fallback) = unlabeled
                && !has_error
            {
                problems.push(format!(
                    "node {id:?}: no `error` edge, so a failure follows the unlabeled edge \
                     to {:?} as though it had succeeded",
                    fallback.target
                ));
            }
        }
    }

    // Reachability. `walk_bfs` visits exactly what a run can arrive at.
    let mut reachable = HashSet::new();
    metalcraft_flows::walk_bfs(&flow.flow, |n| {
        reachable.insert(n.id.clone());
    });
    for node in &flow.flow.nodes {
        if !reachable.contains(&node.id) {
            problems.push(format!("node {:?} is unreachable from entry", node.id));
        }
    }

    for name in interpolated_names(&serde_json::to_value(&flow.flow).unwrap_or(Value::Null)) {
        if !produced.contains(&name) {
            problems.push(format!(
                "`{{{{{name}}}}}` is interpolated but nothing declares it — a missing path \
                 resolves to the empty string, silently"
            ));
        }
    }

    problems
}

/// The root name of every `{{a.b.c}}` anywhere in a JSON subtree.
fn interpolated_names(value: &Value) -> Vec<String> {
    let mut found = Vec::new();
    collect(value, &mut found);
    found.sort();
    found.dedup();
    return found;

    fn collect(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::String(s) => {
                let mut rest = s.as_str();
                while let Some(open) = rest.find("{{") {
                    let after = &rest[open + 2..];
                    let Some(close) = after.find("}}") else { break };
                    let path = after[..close].trim();
                    if !path.is_empty()
                        && path
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
                    {
                        out.push(path.split('.').next().unwrap_or(path).to_string());
                    }
                    rest = &after[close + 2..];
                }
            }
            Value::Array(a) => a.iter().for_each(|v| collect(v, out)),
            Value::Object(o) => o.values().for_each(|v| collect(v, out)),
            _ => {}
        }
    }
}
