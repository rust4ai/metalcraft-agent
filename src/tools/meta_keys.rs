//! Meta tools for managing the **API key / secret store** by prompt — the
//! workshop GUI's key-store surface, driven by tool calls. HTTP-API tools
//! reference these secrets via `$NAME` placeholders, so setting a key here is
//! what lets an enabled integration actually authenticate.
//!
//! Values only ever flow *inward*: `key_set` takes a raw value, but `key_list`
//! returns only [`crate::key_store::mask`]ed previews — never the raw secret —
//! mirroring the HTTP API. These delegate to [`crate::key_store`] and
//! [`crate::integrations::recommended_env`].

use async_trait::async_trait;

use crate::key_store::KeyStore;
use crate::paths;
use crate::tools::missing_param;

fn name_arg(args: &serde_json::Value, tool: &str) -> metalcraft::Result<String> {
    args["name"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| missing_param(tool, "name"))
}

pub struct KeyListTool;

#[async_trait]
impl metalcraft::Tool for KeyListTool {
    fn name(&self) -> &str {
        "key_list"
    }
    fn description(&self) -> &str {
        "List API keys / secrets in the key store. Returns `configured` keys (name + masked preview only — never the raw value) and `recommended` keys that enabled integrations need, each flagged configured/missing so you know what still has to be set with key_set."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }
    async fn call(&self, _args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let configured: Vec<serde_json::Value> = KeyStore::load(&paths::keys_file())
            .list_masked()
            .into_iter()
            .map(|(name, masked)| serde_json::json!({ "name": name, "masked": masked }))
            .collect();
        let recommended: Vec<serde_json::Value> = crate::integrations::recommended_env()
            .into_iter()
            .map(|(name, packs)| {
                serde_json::json!({
                    "name": name,
                    "packs": packs,
                    "configured": crate::key_store::lookup_present(&name).is_some(),
                    "managed": crate::key_store::is_env_authoritative(&name),
                })
            })
            .collect();
        Ok(serde_json::json!({ "configured": configured, "recommended": recommended }))
    }
}

pub struct KeySetTool;

#[async_trait]
impl metalcraft::Tool for KeySetTool {
    fn name(&self) -> &str {
        "key_set"
    }
    fn description(&self) -> &str {
        "Set (create or overwrite) an API key / secret in the key store by name. The value is stored so HTTP-API tools can reference it via `$NAME`. The response masks the value — the raw secret is never echoed back."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Key name, e.g. \"LINEAR_API_KEY\" (the $NAME tools reference)" },
                "value": { "type": "string", "description": "The raw secret value to store" }
            },
            "required": ["name", "value"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let name = name_arg(&args, "key_set")?;
        if name.trim().is_empty() {
            return Ok(serde_json::json!({ "error": "key name must not be empty" }));
        }
        let value = args["value"]
            .as_str()
            .ok_or_else(|| missing_param("key_set", "value"))?;
        // `trim`: a whitespace-only value passes `is_empty` and then behaves like a
        // credential everywhere downstream — including shadowing a fallback the pod
        // could have used. Store nothing rather than something blank.
        if value.trim().is_empty() {
            return Ok(serde_json::json!({ "error": "key value must not be empty" }));
        }
        let path = paths::keys_file();
        let mut store = KeyStore::load(&path);
        store.upsert(&name, value);
        match store.save(&path) {
            Ok(()) => Ok(serde_json::json!({
                "saved": name,
                "masked": crate::key_store::mask(value),
            })),
            Err(e) => Ok(serde_json::json!({ "error": format!("failed to write key store: {e}") })),
        }
    }
}

pub struct KeyDeleteTool;

#[async_trait]
impl metalcraft::Tool for KeyDeleteTool {
    fn name(&self) -> &str {
        "key_delete"
    }
    fn description(&self) -> &str {
        "Delete an API key / secret from the key store by name. Returns an error if no such key is set."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string", "description": "Key name to delete" } },
            "required": ["name"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let name = name_arg(&args, "key_delete")?;
        let path = paths::keys_file();
        let mut store = KeyStore::load(&path);
        if !store.delete(&name) {
            return Ok(serde_json::json!({ "error": format!("key '{name}' not found") }));
        }
        match store.save(&path) {
            Ok(()) => Ok(serde_json::json!({ "deleted": name })),
            Err(e) => Ok(serde_json::json!({ "error": format!("failed to write key store: {e}") })),
        }
    }
}
