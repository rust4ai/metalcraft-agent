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
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct DependencyReport {
    /// Integration packs the flow requires — the union of what the graph
    /// references (`sub_agent.pack` + custom vendor nodes) and what the flow's
    /// `requires` block declares.
    pub required_packs: Vec<String>,
    /// Of `required_packs`, those not installed *and enabled* on this agent.
    pub missing_packs: Vec<String>,
    /// Version/hash conflicts: a required pack is installed & enabled but its
    /// version is outside the declared range, or its content hash doesn't match a
    /// pin. Human-readable, one per conflict (advisory — install never hard-fails).
    pub version_conflicts: Vec<String>,
    /// Tool names the flow's `tool` nodes invoke (the real API surface).
    pub required_tools: Vec<String>,
    /// Personas the flow's nodes run as (`data.persona`).
    pub required_personas: Vec<String>,
    /// Of `required_personas`, those not available on this agent.
    pub missing_personas: Vec<String>,
    /// Secret keys the required (installed) packs declare in their `requires_env`.
    pub required_env: Vec<String>,
}

/// Summary of the flow that was written.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct InstalledFlow {
    pub id: String,
    pub name: String,
    pub node_count: usize,
    /// Whether the flow is enabled for scheduling. Registry flows install disabled.
    pub enabled: bool,
}

/// The result of a successful install: what landed + what it still needs.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct InstallResult {
    pub flow: InstalledFlow,
    pub dependencies: DependencyReport,
    /// The agent preset this flow was bound to, chosen because its roster covers
    /// every persona the flow names. `None` means no installed preset can reach them
    /// all — the flow is saved and runnable by hand, but cannot be armed until one
    /// can, which is worth saying at install rather than discovering later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_preset: Option<String>,
}

/// The integration-pack ids a flow requires — the union of what the graph
/// references (via [`metalcraft_flows::derive_requires`]) and what the flow's
/// `requires` block declares. Used by the uninstall path to find flows that would
/// break if a pack were removed.
pub fn required_packs(flow: &metalcraft_flows::SavedFlow) -> Vec<String> {
    let mut ids: BTreeSet<String> = metalcraft_flows::derive_requires(flow)
        .packs
        .into_iter()
        .map(|p| p.id)
        .collect();
    if let Some(req) = &flow.requires {
        for p in &req.packs {
            ids.insert(p.id.clone());
        }
    }
    ids.into_iter().collect()
}

/// The flow's declared requirements if present, otherwise the shape derived from
/// its graph. This is what enforcement checks against.
fn effective_requires(flow: &metalcraft_flows::SavedFlow) -> metalcraft_flows::Requires {
    flow.requires
        .clone()
        .unwrap_or_else(|| metalcraft_flows::derive_requires(flow))
}

/// Fill in pack requirements for the flow's `tool` nodes using the registry's
/// tool → pack index, so a flow that binds a bare `tool_name` (not a
/// `sub_agent.pack` or `vendor:` node) still records the pack that provides it.
/// Best-effort: a registry error leaves `requires` unchanged (enforcement then
/// falls back to whatever was derivable locally). Prefers a verified provider on
/// a name clash; adds each provider as an unconstrained (`version = None`)
/// requirement, since the index maps identity, not a compatible range.
async fn enrich_requires_from_tools(requires: &mut metalcraft_flows::Requires) {
    if requires.tools.is_empty() {
        return;
    }
    let map = match crate::registry::resolve_tools(&requires.tools).await {
        Ok(m) => m,
        Err(e) => {
            log::warn!("tool→pack enrichment skipped (registry resolve failed): {e}");
            return;
        }
    };
    let mut have: std::collections::BTreeSet<String> =
        requires.packs.iter().map(|p| p.id.clone()).collect();
    for providers in map.values() {
        let Some(chosen) = providers.iter().find(|p| p.verified).or_else(|| providers.first())
        else {
            continue;
        };
        if have.insert(chosen.slug.clone()) {
            requires.packs.push(metalcraft_flows::PackRequirement::new(&chosen.slug));
        }
    }
    requires.packs.sort_by(|a, b| a.id.cmp(&b.id));
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
    use metalcraft_flows::Unmet;

    let requires = effective_requires(flow);
    let required_packs = required_packs(flow);
    let required_tools = requires.tools.clone();

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

    // Version/hash conflicts against enabled packs. Hashing every enabled pack is
    // only worth it when a requirement actually pins a hash.
    let need_hash = requires.packs.iter().any(|p| p.content_sha256.is_some());
    let available: Vec<metalcraft_flows::AvailablePack> = crate::integration_packs::enabled_packs()
        .into_iter()
        .map(|pack| metalcraft_flows::AvailablePack {
            content_sha256: if need_hash {
                crate::integration_packs::installed_content_sha256(&pack.manifest.id)
            } else {
                None
            },
            id: pack.manifest.id.clone(),
            version: pack.manifest.version.clone(),
        })
        .collect();
    let version_conflicts = metalcraft_flows::check_requirements(&requires, &available)
        .into_iter()
        .filter_map(|u| match u {
            Unmet::VersionConflict { id, need, have, .. } => {
                Some(format!("{id}: need {need}, have {have}"))
            }
            Unmet::HashMismatch { id, .. } => {
                Some(format!("{id}: installed content hash does not match the pinned hash"))
            }
            // Missing packs are reported separately in `missing_packs`.
            Unmet::MissingPack { .. } | Unmet::MissingTool { .. } => None,
        })
        .collect();

    let available_personas = crate::persona::Persona::list_available(&paths::personas_dir());
    let required_personas = scan_personas(flow);
    let missing_personas = required_personas
        .iter()
        .filter(|p| !available_personas.iter().any(|a| a == *p))
        .cloned()
        .collect();

    DependencyReport {
        required_packs,
        missing_packs,
        version_conflicts,
        required_tools,
        required_personas,
        missing_personas,
        required_env: required_env.into_iter().collect(),
    }
}

/// Human-readable warnings for running `flow` against this agent's *current* state —
/// empty when everything it needs is installed and enabled. Recomputed at run time (not
/// just install time) and surfaced in the flow-run output so a run that will misbehave
/// for lack of a pack, persona, or a version/hash mismatch says so up front.
pub fn runtime_warnings(flow: &metalcraft_flows::SavedFlow) -> Vec<String> {
    let report = dependency_report(flow);
    let mut out = Vec::new();
    if !report.missing_packs.is_empty() {
        out.push(format!(
            "Missing or disabled packs: {} — install and enable them (Packs app) or this flow's tools won't run.",
            report.missing_packs.join(", ")
        ));
    }
    if !report.missing_personas.is_empty() {
        out.push(format!(
            "Missing personas: {} — the flow references personas this agent doesn't have.",
            report.missing_personas.join(", ")
        ));
    }
    for conflict in &report.version_conflicts {
        out.push(format!("Pack version/hash conflict — {conflict}."));
    }
    out
}

/// Outcome of trying to satisfy one pack requirement of a flow.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct PackInstallOutcome {
    /// The pack id.
    pub pack: String,
    /// The version installed (or already present), when known.
    pub version: Option<String>,
    /// `"installed" | "already-satisfied" | "skipped" | "failed"`.
    pub status: String,
    /// Human-readable detail (why it was skipped/failed, or the resolved version).
    pub detail: Option<String>,
}

/// Whether an installed pack already satisfies a requirement (enabled + version
/// range + hash pin). Delegates to the flows crate so the semver/hash rules stay
/// in one place.
fn requirement_satisfied(
    pr: &metalcraft_flows::PackRequirement,
    installed: &crate::integration_packs::Pack,
) -> bool {
    if !crate::integration_packs::is_enabled(&pr.id) {
        return false;
    }
    let available = metalcraft_flows::AvailablePack {
        id: installed.manifest.id.clone(),
        version: installed.manifest.version.clone(),
        content_sha256: if pr.content_sha256.is_some() {
            crate::integration_packs::installed_content_sha256(&pr.id)
        } else {
            None
        },
    };
    let one = metalcraft_flows::Requires {
        packs: vec![pr.clone()],
        tools: vec![],
    };
    metalcraft_flows::check_requirements(&one, std::slice::from_ref(&available)).is_empty()
}

/// Resolve, download (at the resolved version), hash-verify, install, and enable a
/// single pack requirement — the active "three hops" (registry resolve → versioned
/// download → hash-verified install). No-ops when the requirement is already
/// satisfied; refuses to fetch a built-in pack from the registry.
pub async fn install_pack_requirement(
    pr: &metalcraft_flows::PackRequirement,
) -> PackInstallOutcome {
    let outcome = |status: &str, version: Option<String>, detail: Option<String>| PackInstallOutcome {
        pack: pr.id.clone(),
        version,
        status: status.to_string(),
        detail,
    };

    // Built-in packs are app-managed and can't be pulled from the registry.
    if crate::seed::is_embedded_pack(&pr.id) {
        if let Some(installed) = crate::integration_packs::find_installed(&pr.id) {
            if requirement_satisfied(pr, &installed) {
                return outcome("already-satisfied", Some(installed.manifest.version), None);
            }
            if let Err(e) = crate::integration_packs::set_enabled(&pr.id, true) {
                return outcome("failed", None, Some(e));
            }
            return outcome("installed", Some(installed.manifest.version), Some("enabled built-in pack".into()));
        }
        return outcome("skipped", None, Some("built-in pack is not installed on this agent".into()));
    }

    if let Some(installed) = crate::integration_packs::find_installed(&pr.id) {
        if requirement_satisfied(pr, &installed) {
            return outcome("already-satisfied", Some(installed.manifest.version), None);
        }
    }

    // Hop 1: resolve the range to a concrete version + hash.
    let (version, resolved_hash) =
        match crate::registry::resolve_pack_version(&pr.id, pr.version.as_deref()).await {
            Ok(v) => v,
            Err(e) => return outcome("failed", None, Some(e)),
        };

    // If the flow pinned a hash, the registry's resolved bytes must match it.
    let expected_hash = match &pr.content_sha256 {
        Some(pin) if !pin.eq_ignore_ascii_case(&resolved_hash) => {
            return outcome(
                "failed",
                Some(version),
                Some(format!("resolved content hash {resolved_hash} does not match the pinned {pin}")),
            );
        }
        Some(pin) => pin.clone(),
        None => resolved_hash,
    };

    // Hop 2: download that exact version.
    let bytes = match crate::registry::fetch_zip(&pr.id, Some(&version)).await {
        Ok(b) => b,
        Err(e) => return outcome("failed", Some(version), Some(e)),
    };

    // Hop 3: install with hash verification, then enable.
    if let Err(e) = crate::integration_packs::install_from_zip(&bytes, Some(&expected_hash)) {
        return outcome("failed", Some(version), Some(e));
    }
    if let Err(e) = crate::integration_packs::set_enabled(&pr.id, true) {
        return outcome("failed", Some(version), Some(e));
    }
    outcome("installed", Some(version), None)
}

/// Satisfy every pack a flow's requirements declare, installing + enabling those
/// that are missing or out of range/hash. Returns one outcome per pack.
pub async fn install_flow_dependencies(
    flow: &metalcraft_flows::SavedFlow,
) -> Vec<PackInstallOutcome> {
    let requires = effective_requires(flow);
    let mut out = Vec::new();
    for pr in &requires.packs {
        out.push(install_pack_requirement(pr).await);
    }
    out
}

/// Download flow `slug` from the registry, validate it, and save it into the
/// agent's `flows/` dir (disabled unless the document says otherwise). Returns the
/// installed-flow summary plus the dependency report.
pub async fn install_flow_from_registry(slug: &str) -> Result<InstallResult, String> {
    let slug = slug.trim();
    if slug.is_empty() {
        return Err("slug is required".to_string());
    }

    let mut flow = registry::fetch_flow(slug).await?;

    let errors = metalcraft_flows::validate(&flow);
    if !errors.is_empty() {
        let msg = errors.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
        return Err(format!("flow failed validation: {msg}"));
    }

    // Persist the dependency shape if the author didn't declare one, so the
    // installed flow carries a record of what it needs. We never overwrite an
    // author-provided `requires` (that is their compatibility contract). When we
    // derive it, also enrich pack coverage from the registry's tool→pack index so
    // a bare-`tool`-node flow records the packs its tools come from. Done before
    // the dependency report so the report reflects the resolved packs.
    if flow.requires.is_none() {
        let mut derived = metalcraft_flows::derive_requires(&flow);
        enrich_requires_from_tools(&mut derived).await;
        if !derived.is_empty() {
            flow.requires = Some(derived);
        }
    }

    // Non-destructive upgrade: if the flow is already installed and the user has
    // customized its schedules, keep their schedules rather than clobbering them
    // with the published defaults. The published `schedules` seed only a *fresh*
    // install (or an upgrade the user never touched the schedule on). Mirrors how
    // we treat an author `requires` block without overwriting user intent.
    if let Some(existing) = metalcraft_flows::load_flow(&paths::flows_dir(), &flow.id) {
        if !existing.schedules.is_empty() {
            log::info!(
                "Preserving {} existing schedule(s) on re-install of flow '{}'",
                existing.schedules.len(),
                flow.id
            );
            flow.schedules = existing.schedules;
        }
    }

    let dependencies = dependency_report(&flow);

    metalcraft_flows::save_flow(&paths::flows_dir(), &flow).map_err(|e| e.to_string())?;

    // Bind it to an agent that can actually run it.
    //
    // A flow may only name personas from its preset's roster, and the default agent
    // is deliberately small — so a flow calling a specialist (`morning-briefer`,
    // say) is unarmable until someone works out which preset covers it. Choosing
    // here turns that from a puzzle the user hits at arm time into a line in the
    // install report.
    let bound_preset = crate::flow_bindings::bind_to_a_capable_preset(&flow);

    Ok(InstallResult {
        flow: InstalledFlow {
            id: flow.id.clone(),
            name: flow.name.clone(),
            node_count: flow.flow.nodes.len(),
            enabled: flow.enabled,
        },
        dependencies,
        agent_preset: bound_preset,
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
    fn required_packs_from_sub_agent_and_vendor_nodes() {
        let flow = flow_from(serde_json::json!([
            { "id": "a", "node_type": "sub_agent", "data": { "task": "x", "pack": "linear" } },
            { "id": "b", "node_type": "slack:send_message", "data": {} },
            { "id": "c", "node_type": "prompt", "data": { "prompt": "hi" } },
            { "id": "d", "node_type": "sub_agent", "data": { "task": "y", "pack": "linear" } }
        ]));
        assert_eq!(required_packs(&flow), vec!["linear".to_string(), "slack".to_string()]);
    }

    #[test]
    fn required_packs_unions_declared_requires() {
        // A declared `requires` pack that the graph doesn't reference is still
        // counted (e.g. a tool-node pack the author stamped).
        let mut flow = flow_from(serde_json::json!([
            { "id": "a", "node_type": "sub_agent", "data": { "task": "x", "pack": "linear" } }
        ]));
        flow.requires = Some(metalcraft_flows::Requires {
            packs: vec![metalcraft_flows::PackRequirement::new("cloudflare")],
            tools: vec!["cloudflare_purge_cache".into()],
        });
        assert_eq!(
            required_packs(&flow),
            vec!["cloudflare".to_string(), "linear".to_string()]
        );
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
        assert!(required_packs(&flow).is_empty());
        assert!(scan_personas(&flow).is_empty());
    }
}
