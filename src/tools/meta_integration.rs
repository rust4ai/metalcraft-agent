//! Meta tools for managing **integration packs** by prompt — the workshop GUI's
//! "Integrations" surface (list + enable/disable), driven by tool calls so an
//! agent can install a capability for itself. Enabling a pack makes its
//! personas, skills, HTTP-API tools, and flow templates resolvable; most packs
//! additionally need an API key in the key store (see [`super::meta_keys`]).
//!
//! These delegate to [`crate::integration_packs`] so behaviour matches the
//! HTTP API's pack handlers exactly.

use async_trait::async_trait;

use crate::tools::missing_param;

fn id_arg(args: &serde_json::Value, tool: &str) -> metalcraft::Result<String> {
    args["id"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| missing_param(tool, "id"))
}

/// Build the wire summary for one pack: identity + enabled state + the env keys
/// it needs, each flagged configured/missing so the caller knows whether it
/// still has to set a key after enabling.
fn pack_summary(pack: &crate::integration_packs::Pack) -> serde_json::Value {
    let id = &pack.manifest.id;
    let requires_env: Vec<serde_json::Value> = pack
        .manifest
        .requires_env
        .iter()
        .map(|name| {
            serde_json::json!({
                "name": name,
                "configured": crate::key_store::lookup(name).is_some(),
            })
        })
        .collect();
    serde_json::json!({
        "id": id,
        "name": pack.manifest.name,
        "description": pack.manifest.description,
        "version": pack.manifest.version,
        "enabled": crate::integration_packs::is_enabled(id),
        "requires_env": requires_env,
    })
}

pub struct PackListTool;

#[async_trait]
impl metalcraft::Tool for PackListTool {
    fn name(&self) -> &str {
        "pack_list"
    }
    fn description(&self) -> &str {
        "List all installed integration packs with id, name, description, version, enabled state, and the env keys each requires (each flagged configured/missing). Use this to discover what can be enabled and which API keys still need setting."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }
    async fn call(&self, _args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let packs: Vec<serde_json::Value> = crate::integration_packs::list_installed()
            .iter()
            .map(pack_summary)
            .collect();
        Ok(serde_json::json!({ "packs": packs }))
    }
}

pub struct PackReadTool;

#[async_trait]
impl metalcraft::Tool for PackReadTool {
    fn name(&self) -> &str {
        "pack_read"
    }
    fn description(&self) -> &str {
        "Read one integration pack's full details by id: its manifest, enabled state, the env keys it requires (each flagged configured/missing), the personas/skills/tools/flow-templates it provides, and its README — the setup guide covering which credential to get, how to obtain it, and any provider-side steps. Use this before enabling a pack to walk the user through what it needs and how to set it up."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Integration pack id (e.g. \"discord_admin\", \"github\")" }
            },
            "required": ["id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = id_arg(&args, "pack_read")?;
        let Some(pack) = crate::integration_packs::find_installed(&id) else {
            return Ok(serde_json::json!({ "error": format!("pack '{id}' not installed") }));
        };
        let mut summary = pack_summary(&pack);
        summary["personas"] = serde_json::json!(pack.item_slugs("personas", "json"));
        summary["skills"] = serde_json::json!(pack.item_slugs("skills", "md"));
        summary["tools"] = serde_json::json!(pack.item_slugs("api_tools", "json"));
        summary["flow_templates"] = serde_json::json!(pack.item_slugs("flow_templates", "json"));
        summary["readme"] = match pack.readme() {
            Some(text) => serde_json::Value::String(text),
            None => serde_json::Value::Null,
        };
        Ok(summary)
    }
}

pub struct PackEnableTool;

#[async_trait]
impl metalcraft::Tool for PackEnableTool {
    fn name(&self) -> &str {
        "pack_enable"
    }
    fn description(&self) -> &str {
        "Enable (install) or disable an integration pack by id. Defaults to enabling. Enabling makes the pack's personas, skills, and tools available; check the returned `requires_env` and use key_set to provide any missing API keys. Returns an error if the pack id is not installed."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Integration pack id (e.g. \"linear\", \"github\")" },
                "enabled": { "type": "boolean", "description": "true to enable/install (default), false to disable" }
            },
            "required": ["id"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let id = id_arg(&args, "pack_enable")?;
        let enabled = args["enabled"].as_bool().unwrap_or(true);
        match crate::integration_packs::set_enabled(&id, enabled) {
            Ok(()) => {
                // Re-read the pack so the caller sees the new state plus which
                // required keys are still missing.
                let summary = crate::integration_packs::list_installed()
                    .iter()
                    .find(|p| p.manifest.id == id)
                    .map(pack_summary)
                    .unwrap_or(serde_json::Value::Null);
                Ok(serde_json::json!({ "id": id, "enabled": enabled, "pack": summary }))
            }
            Err(e) => Ok(serde_json::json!({ "error": e })),
        }
    }
}
