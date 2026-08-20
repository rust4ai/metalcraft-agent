//! The `agentpack_*` tools — how an agent installs, inspects and packages agents.
//!
//! Reads auto-approve; anything that writes is approval-gated as
//! [`OperationKind::AgentPackWrite`](crate::approval::OperationKind), which is its own
//! kind rather than `MetaWrite`: installing adds personas whose prompts the model then
//! follows, and vendors tools that run with the operator's credentials.
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{export, find, install, list, uninstall};

fn fail(tool: &str, message: impl Into<String>) -> metalcraft::GraphError {
    metalcraft::GraphError::ToolCallFailed { tool: tool.into(), message: message.into() }
}

pub const TOOL_NAMES: &[&str] = &[
    "agentpack_list",
    "agentpack_read",
    "agentpack_install",
    "agentpack_update",
    "agentpack_uninstall",
    "agentpack_export",
];

pub struct AgentPackListTool;

#[async_trait]
impl metalcraft::Tool for AgentPackListTool {
    fn name(&self) -> &str {
        "agentpack_list"
    }
    fn description(&self) -> &str {
        "List the agent packs installed on this pod. An agent pack provides an agent \
         preset plus every persona, skill and integration it needs."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _args: Value) -> metalcraft::Result<Value> {
        let packs: Vec<Value> = list()
            .into_iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "name": p.manifest.name,
                    "version": p.manifest.version,
                    "presets": p.manifest.presets,
                    "personas": p.manifest.provides.personas.len(),
                    "skills": p.manifest.provides.skills.len(),
                    "integrations": p.manifest.provides.integrations.len(),
                })
            })
            .collect();
        Ok(json!({ "count": packs.len(), "agent_packs": packs }))
    }
}

pub struct AgentPackReadTool;

#[async_trait]
impl metalcraft::Tool for AgentPackReadTool {
    fn name(&self) -> &str {
        "agentpack_read"
    }
    fn description(&self) -> &str {
        "Read an installed agent pack's manifest: what it provides, which domains its \
         tools can reach, and which credentials it needs."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The agent pack id." }
            },
            "required": ["id"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        let id = args["id"].as_str().ok_or_else(|| crate::tools::missing_param("agentpack_read", "id"))?;
        let pack = find(id).ok_or_else(|| fail("agentpack_read", format!("'{id}' is not installed")))?;
        Ok(json!({
            "id": pack.id,
            "manifest": pack.manifest,
            "root": pack.root,
        }))
    }
}

pub struct AgentPackInstallTool;

#[async_trait]
impl metalcraft::Tool for AgentPackInstallTool {
    fn name(&self) -> &str {
        "agentpack_install"
    }
    fn description(&self) -> &str {
        "Install an agent pack from a local .agentpack file. The archive is verified \
         against its own content hash and validated before anything is written: every \
         persona, skill and integration it names must be inside it. Returns what \
         was installed, which domains the agent can now reach, and any credentials the \
         pod is still missing."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to a .agentpack file on this machine."
                }
            },
            "required": ["path"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| crate::tools::missing_param("agentpack_install", "path"))?;
        let bytes = std::fs::read(path)
            .map_err(|e| fail("agentpack_install", format!("reading {path}: {e}")))?;
        let report = install(&bytes, "bundle").map_err(|e| fail("agentpack_install", e))?;
        let mut out = serde_json::to_value(&report)
            .map_err(|e| fail("agentpack_install", e.to_string()))?;
        if !report.missing_env.is_empty() {
            out["note"] = json!(format!(
                "Installed, but these credentials are not set yet: {}. Tools that need them \
                 will error until you add them with key_set.",
                report.missing_env.join(", ")
            ));
        }
        Ok(out)
    }
}

pub struct AgentPackUpdateTool;

#[async_trait]
impl metalcraft::Tool for AgentPackUpdateTool {
    fn name(&self) -> &str {
        "agentpack_update"
    }
    fn description(&self) -> &str {
        "Update an installed agent pack to a newer .agentpack, then report what \
         followed. Live agents made from it pick up the new personas, tools, skills \
         and shipped knowledge; what they have learned, their conversations and their \
         names are never touched. Says explicitly when an agent's persona was \
         withdrawn (it falls back to the preset's default) or its whole preset was \
         (it is orphaned, keeping its memory, and flagged)."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the newer .agentpack file on this machine."
                }
            },
            "required": ["path"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        let path = args["path"]
            .as_str()
            .ok_or_else(|| crate::tools::missing_param("agentpack_update", "path"))?;
        let bytes = std::fs::read(path)
            .map_err(|e| fail("agentpack_update", format!("reading {path}: {e}")))?;
        let report = super::update(&bytes, "bundle").map_err(|e| fail("agentpack_update", e))?;
        let mut out = serde_json::to_value(&report)
            .map_err(|e| fail("agentpack_update", e.to_string()))?;

        // The two edge cases are the whole reason this tool is separate from
        // install, so they get said in words rather than left in a struct.
        let mut notes: Vec<String> = Vec::new();
        for f in &report.personas_fell_back {
            notes.push(format!(
                "'{}' was using persona '{}', which this version removed — it is now using '{}'.",
                f.name, f.from, f.to
            ));
        }
        for o in &report.orphaned {
            notes.push(format!(
                "'{}' was made from preset '{}', which this version removed. It keeps its memory \
                 and conversations and now runs from a local copy of that preset.",
                o.name, o.agent_preset
            ));
        }
        if !notes.is_empty() {
            out["note"] = json!(notes.join(" "));
        }
        Ok(out)
    }
}

pub struct AgentPackUninstallTool;

#[async_trait]
impl metalcraft::Tool for AgentPackUninstallTool {
    fn name(&self) -> &str {
        "agentpack_uninstall"
    }
    fn description(&self) -> &str {
        "Remove an installed agent pack. Refuses while a saved agent still uses one of \
         its presets, because those agents hold memories and conversations; pass \
         force=true to orphan them deliberately."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The agent pack id." },
                "force": {
                    "type": "boolean",
                    "description": "Uninstall even if saved agents depend on it, orphaning them."
                }
            },
            "required": ["id"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| crate::tools::missing_param("agentpack_uninstall", "id"))?;
        let force = args["force"].as_bool().unwrap_or(false);
        let report = uninstall(id, force).map_err(|e| fail("agentpack_uninstall", e))?;
        serde_json::to_value(report).map_err(|e| fail("agentpack_uninstall", e.to_string()))
    }
}

pub struct AgentPackExportTool;

#[async_trait]
impl metalcraft::Tool for AgentPackExportTool {
    fn name(&self) -> &str {
        "agentpack_export"
    }
    fn description(&self) -> &str {
        "Package an agent preset that already exists on this pod into a self-contained \
         .agentpack file — its personas, skills, seed memories and the integrations \
         it declares. This is how you author an agent locally and then install it \
         elsewhere or publish it. It does not include anything a running agent has learned."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "preset": { "type": "string", "description": "The agent preset slug to package." },
                "version": {
                    "type": "string",
                    "description": "Version for the exported pack (semver). Defaults to 0.1.0."
                },
                "out": {
                    "type": "string",
                    "description": "Where to write the .agentpack file."
                }
            },
            "required": ["preset", "out"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        let preset = args["preset"]
            .as_str()
            .ok_or_else(|| crate::tools::missing_param("agentpack_export", "preset"))?;
        let out = args["out"]
            .as_str()
            .ok_or_else(|| crate::tools::missing_param("agentpack_export", "out"))?;
        let version = args["version"].as_str().unwrap_or("0.1.0");

        let bytes = export(preset, version).map_err(|e| fail("agentpack_export", e))?;
        if let Some(parent) = std::path::Path::new(out).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| fail("agentpack_export", format!("creating {}: {e}", parent.display())))?;
        }
        std::fs::write(out, &bytes)
            .map_err(|e| fail("agentpack_export", format!("writing {out}: {e}")))?;

        // Read it straight back so the caller learns now, not at install time on
        // someone else's machine, whether what they just built is valid.
        let parsed = super::Bundle::read(&bytes).map_err(|e| fail("agentpack_export", e))?;
        Ok(json!({
            "path": out,
            "bytes": bytes.len(),
            "id": parsed.manifest.id,
            "version": parsed.manifest.version,
            "content_sha256": parsed.manifest.content_sha256,
            "presets": parsed.manifest.presets,
            "personas": parsed.manifest.provides.personas,
            "skills": parsed.manifest.provides.skills,
            "integrations": parsed.manifest.provides.integrations
                .iter().map(|p| json!({"id": p.id, "version": p.version})).collect::<Vec<_>>(),
            "domains": parsed.consent.domains,
            "requires_env": parsed.consent.requires_env.iter().map(|e| &e.name).collect::<Vec<_>>(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metalcraft::Tool;

    #[test]
    fn every_declared_tool_name_has_an_implementation() {
        let (a, b, c, d, e, f) = (
            AgentPackListTool,
            AgentPackReadTool,
            AgentPackInstallTool,
            AgentPackUpdateTool,
            AgentPackUninstallTool,
            AgentPackExportTool,
        );
        let built: Vec<&str> = vec![a.name(), b.name(), c.name(), d.name(), e.name(), f.name()];
        assert_eq!(built, TOOL_NAMES);
    }

    #[test]
    fn writes_are_approval_gated_and_reads_are_not() {
        use crate::approval::{OperationKind, PermissionLevel};
        let args = json!({});
        for t in
            ["agentpack_install", "agentpack_update", "agentpack_uninstall", "agentpack_export"]
        {
            assert_eq!(OperationKind::classify(t, &args), OperationKind::AgentPackWrite, "{t}");
        }
        assert_eq!(
            OperationKind::AgentPackWrite.default_permission(),
            PermissionLevel::RequiresApproval
        );
        for t in ["agentpack_list", "agentpack_read"] {
            assert_eq!(OperationKind::classify(t, &args), OperationKind::MetaRead, "{t}");
            assert_eq!(OperationKind::MetaRead.default_permission(), PermissionLevel::AutoApprove);
        }
    }

    #[test]
    fn schemas_mark_their_required_params() {
        for (schema, required) in [
            (AgentPackReadTool.parameters_schema(), vec!["id"]),
            (AgentPackInstallTool.parameters_schema(), vec!["path"]),
            (AgentPackUninstallTool.parameters_schema(), vec!["id"]),
            (AgentPackExportTool.parameters_schema(), vec!["preset", "out"]),
        ] {
            assert_eq!(schema["type"], "object");
            let got: Vec<&str> =
                schema["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
            assert_eq!(got, required);
        }
    }
}
