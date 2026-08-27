//! Which **preset** a flow belongs to.
//!
//! A flow's `prompt` / `branch` / `sub_agent` nodes each name a persona, and nothing
//! constrained *which* — a flow could reach any persona on the pod, which is the one
//! place the containment rule enforced everywhere else did not reach. Binding a flow
//! to an [agent preset](crate::agent_preset) closes that: a flow may only name
//! personas from its preset's roster.
//!
//! ## Why this lives beside the flow rather than inside it
//!
//! `SavedFlow` is a published type in the `metalcraft-flows` crate, so it cannot
//! gain a field from here. That is a stopgap: a flow's preset ought to travel with
//! it when published — see the note in `docs/FLOWS_AND_AGENT_PRESETS_PLAN.md` §3.1.
//!
//! **Arming lives in [`crate::scheduled_flows`]**, not here. Which agent runs a
//! schedule is a property of the schedule, and now that a schedule is its own
//! document it can simply hold the instance id. The `instances` map this file used
//! to keep is gone; [`clear_instances`] exists only to drain it during migration.
use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent_preset::{AgentPreset, DEFAULT_PRESET};
use crate::paths;

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct FlowBinding {
    /// The agent preset this flow belongs to. `None` resolves to the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// **Legacy**: `schedule id -> agent instance id`, from before schedules were
    /// their own documents.
    ///
    /// Read once, by the migration in [`crate::scheduled_flows`], to carry each
    /// armed agent onto the [`ScheduledFlow`](metalcraft_flows::ScheduledFlow) that
    /// replaces it; [`clear_instances`] then empties it. Nothing writes to it, and
    /// an empty map is not serialized, so a migrated pod's bindings file stops
    /// mentioning it at all.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
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
    get(flow_id)
        .preset
        .unwrap_or_else(|| DEFAULT_PRESET.to_string())
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

/// Every persona a flow names: the flow-level default on the entry node and each
/// node's own override.
///
/// A *schedule* may also override the persona, but a schedule is no longer part of
/// the flow — so that check belongs to the moment a schedule is created
/// ([`crate::scheduled_flows::arm`]) rather than here, where it would have to guess
/// which schedules exist.
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

/// Drain a flow's legacy `instances` map, keeping its preset.
///
/// Called once per flow by the migration in [`crate::scheduled_flows`], after the
/// agents it named have been written onto the schedules that replace it.
pub fn clear_instances(flow_id: &str) -> Result<(), String> {
    let mut b = load();
    let Some(binding) = b.flows.get_mut(flow_id) else {
        return Ok(());
    };
    if binding.instances.is_empty() {
        return Ok(());
    }
    binding.instances.clear();
    save(&b)
}

/// Forget a flow's binding entirely (on flow delete).
///
/// Its schedules are a separate matter — see
/// [`crate::scheduled_flows::forget_flow`], which the same delete path calls.
pub fn forget(flow_id: &str) -> Result<(), String> {
    let mut b = load();
    b.flows.remove(flow_id);
    save(&b)
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

    // The `has_schedule` tests that used to live here are gone with the function.
    //
    // They pinned "anything the pod lists, the pod can arm" — an invariant that
    // existed because listing a flow's schedules meant *resolving* them through
    // three tiers while arming validated against only the first, so the pod would
    // offer a schedule and then refuse it. There is nothing left to disagree: a
    // schedule is a document that exists or does not, and arming creates one
    // rather than selecting from a synthesized list.

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
