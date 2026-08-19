//! Read tools for **integration packs** — the workshop GUI's "Integrations"
//! surface, driven by tool calls so an agent can inspect its own capabilities.
//!
//! These are read-only. Integration packs are no longer installed or enabled on
//! their own: an **agent pack** is the install unit and vendors the integration
//! packs its personas need (see [`crate::agent_packs`] and
//! `docs/AGENT_PACKS_PLAN.md`). What is left to configure per pack is the API key
//! it authenticates with — see [`super::meta_keys`].
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

/// Build the wire summary for one pack: identity plus the env keys it needs, each
/// flagged configured/missing so the caller knows what is still unset.
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
        // An integration pack is present or absent — there is no enabled state to
        // report since agent packs became the install unit. Kept as an explicit
        // `true` rather than dropped, because a missing field reads as "unknown".
        "installed": true,
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
        "List every installed integration pack with id, name, description, version, and the env keys each requires (flagged configured/missing). Use this to see what capabilities exist and which API keys still need setting. Integration packs arrive as part of an agent pack — to add one, install an agent pack (agentpack_install)."
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
        "Read one integration pack's full details by id: its manifest, the env keys it requires (each flagged configured/missing), the personas/skills/tools/flow-templates it provides, and its README — the setup guide covering which credential to get, how to obtain it, and any provider-side steps. Use this to walk the user through what a pack needs before setting its keys."
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
