//! Which agent a flow runs as.
//!
//! A flow's `prompt` / `branch` / `sub_agent` nodes each name a persona, and nothing
//! constrained *which* — a flow could reach any persona on the pod, which is the one
//! place the containment rule enforced everywhere else did not reach. Binding a flow
//! to an [agent preset](crate::agent_preset) closes that: a flow may only name
//! personas from its preset's roster.
//!
//! Arming a schedule then mints a **persistent agent instance**, and every firing is
//! a conversation inside it — so scheduled work accumulates memory across runs. A
//! morning briefer can notice it said the same thing yesterday.
//!
//! ## Why this lives beside the flow rather than inside it
//!
//! `SavedFlow` and `FlowScheduleSpec` are published types in the `metalcraft-flows`
//! crate, so neither can gain a field from here. That is only a stopgap for `preset`
//! (which ought to travel with a published flow — see the note in
//! `docs/FLOWS_AND_AGENT_PRESETS_PLAN.md` §3.1). For `instance` it is *correct*: an
//! instance id is pod-local and must never be published, or a downloaded flow would
//! arrive carrying somebody else's agent.
use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent_instance::{AgentInstance, InstanceOrigin};
use crate::agent_preset::{AgentPreset, DEFAULT_PRESET};
use crate::paths;

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FlowBinding {
    /// The agent preset this flow belongs to. `None` resolves to the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// `schedule id -> agent instance id`. Populated by [`arm`], never by install.
    #[serde(default)]
    pub instances: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Bindings {
    #[serde(default)]
    flows: HashMap<String, FlowBinding>,
}

fn bindings_file() -> PathBuf {
    paths::data_dir().join("flow_bindings.json")
}

fn load() -> Bindings {
    std::fs::read_to_string(bindings_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save(b: &Bindings) -> Result<(), String> {
    let path = bindings_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(b).map_err(|e| format!("serializing: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("finalizing {}: {e}", path.display()))
}

pub fn get(flow_id: &str) -> FlowBinding {
    load().flows.get(flow_id).cloned().unwrap_or_default()
}

/// The preset a flow runs as. Unbound flows fall back to the default agent, which is
/// what every flow written before presets existed effectively was.
pub fn preset_for(flow_id: &str) -> String {
    get(flow_id).preset.unwrap_or_else(|| DEFAULT_PRESET.to_string())
}

/// Bind a flow to a preset, rejecting the binding if the flow names personas the
/// preset cannot reach — better to fail here than at 3am when the cron fires.
pub fn bind_preset(flow: &metalcraft_flows::SavedFlow, preset_slug: &str) -> Result<(), String> {
    let preset = AgentPreset::load(preset_slug, &paths::agent_presets_dir())?;
    check_personas(flow, &preset)?;
    let mut b = load();
    b.flows.entry(flow.id.clone()).or_default().preset = Some(preset_slug.to_string());
    save(&b)
}

/// Bind a flow to an installed preset whose roster covers every persona it names.
///
/// The default agent is deliberately small, and the containment rule means a flow it
/// cannot reach is a flow nobody can arm. Rather than leaving that to be discovered
/// at arm time — as a message about a persona the user never chose — pick a preset
/// that works, at install, and say which.
///
/// The default wins when it fits, because an unremarkable flow should belong to the
/// unremarkable agent. Otherwise the first preset that can reach everything, by slug
/// order so the choice is stable across runs rather than depending on directory
/// iteration. `None` means nothing installed can run it.
pub fn bind_to_a_capable_preset(flow: &metalcraft_flows::SavedFlow) -> Option<String> {
    let dir = paths::agent_presets_dir();
    let named = personas_named(flow);

    let fits = |slug: &str| {
        AgentPreset::load(slug, &dir)
            .ok()
            .is_some_and(|p| named.iter().all(|n| p.allows_persona(n)))
    };

    let chosen = if fits(DEFAULT_PRESET) {
        Some(DEFAULT_PRESET.to_string())
    } else {
        let mut slugs: Vec<String> = AgentPreset::list_summaries(&dir)
            .into_iter()
            .map(|s| s.slug)
            .filter(|s| s != DEFAULT_PRESET)
            .collect();
        slugs.sort();
        slugs.into_iter().find(|s| fits(s))
    }?;

    // Don't move a flow the operator has already placed. Re-installing a flow they
    // deliberately bound elsewhere must not quietly hand it back to the default.
    if get(&flow.id).preset.is_some() {
        return get(&flow.id).preset;
    }

    match bind_preset(flow, &chosen) {
        Ok(()) => Some(chosen),
        Err(e) => {
            log::warn!("flow '{}': could not bind to '{chosen}': {e}", flow.id);
            None
        }
    }
}

/// Clear a flow's preset, returning it to the default agent. Armed schedules are
/// left alone: the agents already running this flow keep running it, and their
/// memory is not something a rebind should quietly discard.
pub fn unbind(flow_id: &str) -> Result<(), String> {
    let mut b = load();
    if let Some(binding) = b.flows.get_mut(flow_id) {
        binding.preset = None;
    }
    save(&b)
}

/// Every persona a flow names: the flow-level default on the entry node, each node's
/// own override, and each schedule's.
pub fn personas_named(flow: &metalcraft_flows::SavedFlow) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |v: Option<&str>| {
        if let Some(p) = v.filter(|s| !s.is_empty())
            && !out.iter().any(|x| x == p)
        {
            out.push(p.to_string());
        }
    };
    for node in &flow.flow.nodes {
        push(node.data.get("persona").and_then(|v| v.as_str()));
    }
    for s in &flow.schedules {
        push(s.persona.as_deref());
    }
    out
}

/// The containment rule, applied to flows.
pub fn check_personas(
    flow: &metalcraft_flows::SavedFlow,
    preset: &AgentPreset,
) -> Result<(), String> {
    let outside: Vec<String> = personas_named(flow)
        .into_iter()
        .filter(|p| !preset.allows_persona(p))
        .collect();
    if outside.is_empty() {
        return Ok(());
    }
    Err(format!(
        "flow '{}' names persona(s) {} which are not in agent '{}' (roster: {})",
        flow.id,
        outside.join(", "),
        preset.slug,
        preset.callable_personas().join(", ")
    ))
}

/// The instance a schedule runs as, if it has been armed.
pub fn instance_for(flow_id: &str, schedule_id: &str) -> Option<String> {
    get(flow_id).instances.get(schedule_id).cloned()
}

/// Arm a schedule: bind it to a persistent agent, minting one if needed.
///
/// **Arming is what creates the agent.** Installing a pack ships flows disabled, so
/// this is the deliberate "yes, run this in the background" act — the natural moment
/// for the instance to come into existence, and the second consent point after
/// install.
///
/// Schedules of one flow share an agent by default: two crons on the same flow (the
/// 08:00 and 18:00 case) are the same agent, so the evening run remembers the
/// morning one. Pass `instance` to attach to an existing agent instead — running a
/// briefer as the same agent you chat with is a reasonable thing to want.
pub fn arm(
    flow: &metalcraft_flows::SavedFlow,
    schedule_id: &str,
    instance: Option<&str>,
) -> Result<AgentInstance, String> {
    let preset_slug = preset_for(&flow.id);
    let preset = AgentPreset::load(&preset_slug, &paths::agent_presets_dir())?;
    check_personas(flow, &preset)?;

    if !flow.schedules.iter().any(|s| s.id == schedule_id) {
        return Err(format!("flow '{}' has no schedule '{schedule_id}'", flow.id));
    }

    let mut b = load();
    let binding = b.flows.entry(flow.id.clone()).or_default();

    // Explicit target, then another armed schedule of this flow, then a new agent.
    let resolved = match instance {
        Some(id) => {
            // Arming makes an agent do work on a timer, which is the same commitment
            // as naming it. Without this, attaching a schedule to an existing chat's
            // agent left it ephemeral and eligible for reaping — deleting the memory
            // the recurring run was accumulating.
            let mut existing = crate::agent_instance::load(id)?;
            if !existing.persistent {
                existing.persistent = true;
                existing.save()?;
            }
            existing
        }
        None => match binding.instances.values().next().and_then(|id| crate::agent_instance::load(id).ok()) {
            Some(existing) => existing,
            None => {
                let mut i = AgentInstance::new(&preset, InstanceOrigin::Flow { flow_id: flow.id.clone() });
                let label = flow
                    .schedules
                    .iter()
                    .find(|s| s.id == schedule_id)
                    .and_then(|s| s.name.clone())
                    .unwrap_or_else(|| flow.name.clone());
                i.name = format!("{} — {label}", preset.name);
                i.persistent = true;
                i.save()?;
                i
            }
        },
    };

    binding.instances.insert(schedule_id.to_string(), resolved.id.clone());
    if binding.preset.is_none() {
        binding.preset = Some(preset_slug);
    }
    save(&b)?;
    Ok(resolved)
}

/// Disarm a schedule. **Keeps the agent and everything it remembers** — disarming is
/// "stop running this on a timer", not "destroy the thing that was running it".
pub fn disarm(flow_id: &str, schedule_id: &str) -> Result<(), String> {
    let mut b = load();
    if let Some(binding) = b.flows.get_mut(flow_id) {
        binding.instances.remove(schedule_id);
    }
    save(&b)
}

/// Forget a flow's bindings entirely (on flow delete).
pub fn forget(flow_id: &str) -> Result<(), String> {
    let mut b = load();
    b.flows.remove(flow_id);
    save(&b)
}

/// Flows bound to an agent — "what is this thing scheduled to do", which a pod
/// currently cannot answer.
pub fn flows_for_instance(instance_id: &str) -> Vec<(String, Vec<String>)> {
    let b = load();
    let mut out: Vec<(String, Vec<String>)> = b
        .flows
        .into_iter()
        .filter_map(|(flow_id, binding)| {
            let schedules: Vec<String> = binding
                .instances
                .into_iter()
                .filter(|(_, id)| id == instance_id)
                .map(|(sched, _)| sched)
                .collect();
            (!schedules.is_empty()).then_some((flow_id, schedules))
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(json: &str) -> metalcraft_flows::SavedFlow {
        serde_json::from_str(json).expect("parse flow")
    }

    const BRIEF: &str = r#"{
      "spec_version": "2", "id": "brief", "name": "Morning brief",
      "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
      "enabled": false,
      "flow": { "nodes": [
        { "id": "entry", "node_type": "entry", "data": { "persona": "morning-briefer" }, "position": [0,0] },
        { "id": "compose", "node_type": "prompt", "data": { "persona": "amy-shopper", "prompt": "x" }, "position": [1,0] },
        { "id": "send", "node_type": "tool", "data": { "tool_name": "say_to_user" }, "position": [2,0] }
      ], "edges": [] }
    }"#;

    fn preset(personas: &str, default: &str) -> AgentPreset {
        serde_json::from_str(&format!(
            r#"{{"slug":"amy","name":"Amy","default_persona":"{default}","personas":{personas}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn personas_are_collected_from_nodes_and_deduplicated() {
        let f = flow(BRIEF);
        let mut names = personas_named(&f);
        names.sort();
        assert_eq!(names, vec!["amy-shopper", "morning-briefer"]);
    }

    #[test]
    fn a_flow_naming_a_persona_outside_the_roster_is_rejected() {
        let f = flow(BRIEF);
        let p = preset(
            r#"[{"slug":"morning-briefer","role":"default"}]"#,
            "morning-briefer",
        );
        let err = check_personas(&f, &p).expect_err("amy-shopper is not in the roster");
        assert!(err.contains("amy-shopper"), "{err}");
        assert!(err.contains("roster"), "{err}");
    }

    #[test]
    fn a_flow_inside_its_roster_passes() {
        let f = flow(BRIEF);
        let p = preset(
            r#"[{"slug":"morning-briefer","role":"default"},{"slug":"amy-shopper","role":"subagent"}]"#,
            "morning-briefer",
        );
        check_personas(&f, &p).expect("both personas are in the roster");
    }

    #[test]
    fn a_flow_naming_no_personas_is_always_allowed() {
        // Tool-only flows exist and must not need a roster entry to run.
        let f = flow(
            r#"{"spec_version":"2","id":"t","name":"t","created_at":"x","updated_at":"x",
                "flow":{"nodes":[{"id":"n","node_type":"tool","data":{"tool_name":"say_to_user"},"position":[0,0]}],"edges":[]}}"#,
        );
        assert!(personas_named(&f).is_empty());
        let p = preset(r#"[{"slug":"a","role":"default"}]"#, "a");
        check_personas(&f, &p).expect("nothing to contain");
    }
}
