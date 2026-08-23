//! Meta tools for authoring **personas** by prompt — the same persona CRUD the
//! workshop GUI exposes, so an agent can manage the metalcraft project itself.
//! All operate on the user-local personas dir (`paths::personas_dir()`); writes
//! and deletes refuse pack-provided slugs (those are read-only), mirroring the
//! workshop HTTP API's `put_persona`/`delete_persona`.

use async_trait::async_trait;

use crate::paths;
use crate::persona::Persona;
use crate::tools::missing_param;

fn slug_arg(args: &serde_json::Value, tool: &str) -> metalcraft::Result<String> {
    args["slug"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| missing_param(tool, "slug"))
}

/// Return the pack id that owns `slug` when there's no local file shadowing it,
/// i.e. the write/delete must be refused. `None` => the user path may proceed.
fn pack_owner_blocking_write(slug: &str) -> Option<String> {
    let filename = format!("{slug}.json");
    if paths::personas_dir().join(&filename).exists() {
        return None;
    }
    crate::integrations::resolve_file(&paths::personas_dir(), "personas", &filename)
        .and_then(|(_, origin)| origin.pack_id().map(String::from))
}

pub struct PersonaListTool;

#[async_trait]
impl metalcraft::Tool for PersonaListTool {
    fn name(&self) -> &str {
        "persona_list"
    }
    fn description(&self) -> &str {
        "List all personas (local + enabled integrations) with slug, name, description, and whether each is pack-provided (read-only)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {}, "required": [] })
    }
    async fn call(&self, _args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let personas = Persona::list_summaries(&paths::personas_dir());
        Ok(serde_json::json!({ "personas": personas }))
    }
}

pub struct PersonaReadTool;

#[async_trait]
impl metalcraft::Tool for PersonaReadTool {
    fn name(&self) -> &str {
        "persona_read"
    }
    fn description(&self) -> &str {
        "Read one persona by slug, returning its full definition (name, description, tools, packs, skills, system_prompt)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "slug": { "type": "string", "description": "Persona slug (filename without .json)" } },
            "required": ["slug"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let slug = slug_arg(&args, "persona_read")?;
        match Persona::load(&slug, &paths::personas_dir()) {
            Ok(p) => Ok(serde_json::to_value(p).unwrap_or(serde_json::Value::Null)),
            Err(e) => Ok(serde_json::json!({ "error": e })),
        }
    }
}

pub struct PersonaWriteTool;

#[async_trait]
impl metalcraft::Tool for PersonaWriteTool {
    fn name(&self) -> &str {
        "persona_write"
    }
    fn description(&self) -> &str {
        "Create or overwrite a persona. Provide `slug` and `persona`: a JSON string for an object with fields name, description, tools (array), optional packs (array), optional skills (array), and system_prompt. Refuses slugs owned by an integration."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "slug": { "type": "string", "description": "Persona slug to write (filename without .json)" },
                "persona": {
                    "type": "string",
                    "description": "Full persona definition as a JSON string: { name, description, tools[], packs?[], skills?[], system_prompt }"
                }
            },
            "required": ["slug", "persona"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let slug = slug_arg(&args, "persona_write")?;
        let persona_raw = args
            .get("persona")
            .cloned()
            .ok_or_else(|| missing_param("persona_write", "persona"))?;
        // Accept either a JSON string (what the model sends — object-typed tool
        // params are rejected by strict function schemas) or an inline object.
        let persona_value: serde_json::Value = if let Some(s) = persona_raw.as_str() {
            match serde_json::from_str(s) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(serde_json::json!({ "error": format!("invalid persona JSON: {e}") }));
                }
            }
        } else {
            persona_raw
        };

        if let Some(pack_id) = pack_owner_blocking_write(&slug) {
            return Ok(serde_json::json!({
                "error": format!("persona '{slug}' is provided by the '{pack_id}' integration and is read-only. Choose a different slug.")
            }));
        }

        let persona: Persona = match serde_json::from_value(persona_value) {
            Ok(p) => p,
            Err(e) => return Ok(serde_json::json!({ "error": format!("invalid persona: {e}") })),
        };

        match persona.save(&slug, &paths::personas_dir()) {
            Ok(()) => Ok(serde_json::json!({ "saved": slug, "name": persona.name })),
            Err(e) => Ok(serde_json::json!({ "error": e })),
        }
    }
}

pub struct PersonaDeleteTool;

#[async_trait]
impl metalcraft::Tool for PersonaDeleteTool {
    fn name(&self) -> &str {
        "persona_delete"
    }
    fn description(&self) -> &str {
        "Delete a user-local persona by slug. Pack-provided personas cannot be deleted."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "slug": { "type": "string", "description": "Persona slug to delete" } },
            "required": ["slug"]
        })
    }
    async fn call(&self, args: serde_json::Value) -> metalcraft::Result<serde_json::Value> {
        let slug = slug_arg(&args, "persona_delete")?;
        if !paths::personas_dir().join(format!("{slug}.json")).exists() {
            return Ok(serde_json::json!({
                "error": format!("persona '{slug}' is not a user-local persona (pack personas can't be deleted)")
            }));
        }
        match Persona::delete(&slug, &paths::personas_dir()) {
            Ok(()) => Ok(serde_json::json!({ "deleted": slug })),
            Err(e) => Ok(serde_json::json!({ "error": e })),
        }
    }
}
