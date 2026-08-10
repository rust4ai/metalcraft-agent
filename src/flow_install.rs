//! Install a flow from the flows registry (flows.metalcraftai.com): download the
//! `SavedFlow`, validate it against the spec, report the packs/personas/secrets it
//! depends on, and save it into `flows_dir()`.
//!
//! This mirrors the pack install path
//! ([`crate::registry::fetch_zip`] + [`crate::integration_packs::install_from_zip`]),
//! but a flow is a single self-contained JSON document, so there's no ZIP to
//! extract — validate then write one file. The dependency report is advisory: an
//! install never hard-fails on a missing pack (the flow just won't run until the
//! dependency is installed), matching how pack install surfaces `requires_env`.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::{paths, registry};

/// What a flow needs beyond itself, checked against this agent's current state.
#[derive(Debug, Serialize)]
pub struct DependencyReport {
    /// Integration packs the flow references (`sub_agent.pack` + custom vendor nodes).
    pub required_packs: Vec<String>,
    /// Of `required_packs`, those not installed *and enabled* on this agent.
    pub missing_packs: Vec<String>,
    /// Personas the flow's nodes run as (`data.persona`).
    pub required_personas: Vec<String>,
    /// Of `required_personas`, those not available on this agent.
    pub missing_personas: Vec<String>,
    /// Secret keys the required (installed) packs declare in their `requires_env`.
    pub required_env: Vec<String>,
}

/// Summary of the flow that was written.
#[derive(Debug, Serialize)]
pub struct InstalledFlow {
    pub id: String,
    pub name: String,
    pub node_count: usize,
    /// Whether the flow is enabled for scheduling. Registry flows install disabled.
    pub enabled: bool,
}

/// The result of a successful install: what landed + what it still needs.
#[derive(Debug, Serialize)]
pub struct InstallResult {
    pub flow: InstalledFlow,
    pub dependencies: DependencyReport,
}

/// Packs referenced by a flow: a `sub_agent` node's `data.pack`, and the vendor
/// prefix of any custom `vendor:action` node type.
fn scan_packs(flow: &metalcraft_flows::SavedFlow) -> Vec<String> {
    use metalcraft_flows::FlowNodeType;
    let mut packs = BTreeSet::new();
    for node in &flow.flow.nodes {
        if let Some(pack) = node.data.get("pack").and_then(|v| v.as_str()) {
            if !pack.is_empty() {
                packs.insert(pack.to_string());
            }
        }
        if let FlowNodeType::Custom(wire) = &node.node_type {
            if let Some((vendor, _)) = wire.split_once(':') {
                if !vendor.is_empty() {
                    packs.insert(vendor.to_string());
                }
            }
        }
    }
    packs.into_iter().collect()
}

/// Personas referenced by a flow's nodes (`data.persona`).
fn scan_personas(flow: &metalcraft_flows::SavedFlow) -> Vec<String> {
    let mut personas = BTreeSet::new();
    for node in &flow.flow.nodes {
        if let Some(p) = node.data.get("persona").and_then(|v| v.as_str()) {
            if !p.is_empty() {
                personas.insert(p.to_string());
            }
        }
    }
    personas.into_iter().collect()
}

/// Compute the dependency report for an already-parsed flow (no I/O beyond reading
/// the agent's installed packs + personas). Split out so it's unit-testable.
pub fn dependency_report(flow: &metalcraft_flows::SavedFlow) -> DependencyReport {
    let required_packs = scan_packs(flow);
    let mut missing_packs = Vec::new();
    let mut required_env = BTreeSet::new();
    for p in &required_packs {
        match crate::integration_packs::find_installed(p) {
            Some(pack) => {
                for k in &pack.manifest.requires_env {
                    required_env.insert(k.clone());
                }
                if !crate::integration_packs::is_enabled(p) {
                    missing_packs.push(p.clone());
                }
            }
            None => missing_packs.push(p.clone()),
        }
    }

    let available = crate::persona::Persona::list_available(&paths::personas_dir());
    let required_personas = scan_personas(flow);
    let missing_personas = required_personas
        .iter()
        .filter(|p| !available.iter().any(|a| a == *p))
        .cloned()
        .collect();

    DependencyReport {
        required_packs,
        missing_packs,
        required_personas,
        missing_personas,
        required_env: required_env.into_iter().collect(),
    }
}

/// Download flow `slug` from the registry, validate it, and save it into the
/// agent's `flows/` dir (disabled unless the document says otherwise). Returns the
/// installed-flow summary plus the dependency report.
pub async fn install_flow_from_registry(slug: &str) -> Result<InstallResult, String> {
    let slug = slug.trim();
    if slug.is_empty() {
        return Err("slug is required".to_string());
    }

    let flow = registry::fetch_flow(slug).await?;

    let errors = metalcraft_flows::validate(&flow);
    if !errors.is_empty() {
        let msg = errors.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
        return Err(format!("flow failed validation: {msg}"));
    }

    let dependencies = dependency_report(&flow);

    metalcraft_flows::save_flow(&paths::flows_dir(), &flow).map_err(|e| e.to_string())?;

    Ok(InstallResult {
        flow: InstalledFlow {
            id: flow.id.clone(),
            name: flow.name.clone(),
            node_count: flow.flow.nodes.len(),
            enabled: flow.enabled,
        },
        dependencies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow_from(nodes: serde_json::Value) -> metalcraft_flows::SavedFlow {
        let doc = serde_json::json!({
            "spec_version": "2",
            "id": "demo",
            "name": "Demo",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "flow": { "nodes": nodes, "edges": [] }
        });
        serde_json::from_value(doc).unwrap()
    }

    #[test]
    fn scans_packs_from_sub_agent_and_vendor_nodes() {
        let flow = flow_from(serde_json::json!([
            { "id": "a", "node_type": "sub_agent", "data": { "task": "x", "pack": "linear" } },
            { "id": "b", "node_type": "slack:send_message", "data": {} },
            { "id": "c", "node_type": "prompt", "data": { "prompt": "hi" } },
            { "id": "d", "node_type": "sub_agent", "data": { "task": "y", "pack": "linear" } }
        ]));
        assert_eq!(scan_packs(&flow), vec!["linear".to_string(), "slack".to_string()]);
    }

    #[test]
    fn scans_personas() {
        let flow = flow_from(serde_json::json!([
            { "id": "p", "node_type": "prompt", "data": { "prompt": "x", "persona": "triage" } }
        ]));
        assert_eq!(scan_personas(&flow), vec!["triage".to_string()]);
    }

    #[test]
    fn plain_flow_has_no_pack_deps() {
        let flow = flow_from(serde_json::json!([
            { "id": "e", "node_type": "entry", "data": { "schedule_type": "manual" } },
            { "id": "p", "node_type": "prompt", "data": { "prompt": "go" } }
        ]));
        assert!(scan_packs(&flow).is_empty());
        assert!(scan_personas(&flow).is_empty());
    }
}
