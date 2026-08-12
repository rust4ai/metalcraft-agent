//! The 8 `mnote_*` native tools. Each wraps [`NotesStore`] and returns the same
//! `{status, data}` envelope the declarative HTTP tools produce (see
//! `tools::http_api`), so the agent contract is byte-identical — the notes
//! persona/skill need no changes.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::store::NotesStore;
use super::NotesError;

/// Register all 8 tools into the registry.
pub fn register(reg: metalcraft::ToolRegistry, store: NotesStore) -> metalcraft::ToolRegistry {
    reg.register(Whoami(store.clone()))
        .register(ListNotes(store.clone()))
        .register(GetNote(store.clone()))
        .register(CreateNote(store.clone()))
        .register(UpdateNote(store.clone()))
        .register(DeleteNote(store.clone()))
        .register(ListCategories(store.clone()))
        .register(CreateCategory(store))
}

/// Success envelope, matching `HttpApiTool`.
fn ok(status: u16, data: Value) -> Value {
    json!({ "status": status, "data": data })
}

/// Error envelope — a non-2xx status with a JSON error body, matching the cloud.
fn err(e: NotesError) -> Value {
    json!({ "status": e.status, "data": { "error": e.message } })
}

/// Ensure schema/seed, mapping a failure into the error envelope.
async fn ready(store: &NotesStore) -> Result<(), Value> {
    store.ensure_ready().await.map_err(err)
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn present(args: &Value, key: &str) -> bool {
    args.get(key).map_or(false, |v| !v.is_null())
}

fn category_ids(args: &Value) -> Option<Vec<String>> {
    args.get("categories")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
}

// ── mnote_whoami ─────────────────────────────────────────────────────────────
pub struct Whoami(NotesStore);
#[async_trait]
impl metalcraft::Tool for Whoami {
    fn name(&self) -> &str { "mnote_whoami" }
    fn description(&self) -> &str {
        "Validate access and see identity/scope. Returns { sub, email, scopes } (scopes is null for a full-access owner). Takes no parameters."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _args: Value) -> metalcraft::Result<Value> {
        Ok(ok(200, self.0.whoami()))
    }
}

// ── mnote_list_notes ─────────────────────────────────────────────────────────
pub struct ListNotes(NotesStore);
#[async_trait]
impl metalcraft::Tool for ListNotes {
    fn name(&self) -> &str { "mnote_list_notes" }
    fn description(&self) -> &str {
        "List the account's notes (flat — no folders). Returns summaries (no bodies): id, title, slug, is_favorite, timestamps, and categories. Optional `sort` (updated|accessed, default updated) and `category` (a category id) filter."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sort": { "type": "string", "enum": ["updated", "accessed"], "description": "Order newest-first by last edited (updated) or last opened (accessed). Default updated." },
                "category": { "type": "string", "description": "Optional category id to filter by (from mnote_list_categories)." }
            }
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match self.0.list_notes(str_arg(&args, "sort"), str_arg(&args, "category")).await {
            Ok(v) => Ok(ok(200, serde_json::to_value(v).unwrap_or(Value::Null))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mnote_get_note ───────────────────────────────────────────────────────────
pub struct GetNote(NotesStore);
#[async_trait]
impl metalcraft::Tool for GetNote {
    fn name(&self) -> &str { "mnote_get_note" }
    fn description(&self) -> &str {
        "Get one note by `slug`. Returns the full note including its markdown `body`, version, timestamps, and categories. Opening updates last_accessed_at."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "slug": { "type": "string", "description": "Note slug (from mnote_list_notes)." } },
            "required": ["slug"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        let Some(slug) = str_arg(&args, "slug") else {
            return Ok(err(NotesError::bad_request("slug is required")));
        };
        match self.0.get_note(slug).await {
            Ok(v) => Ok(ok(200, serde_json::to_value(v).unwrap_or(Value::Null))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mnote_create_note ────────────────────────────────────────────────────────
pub struct CreateNote(NotesStore);
#[async_trait]
impl metalcraft::Tool for CreateNote {
    fn name(&self) -> &str { "mnote_create_note" }
    fn description(&self) -> &str {
        "Create a note. `title` is the title; `body` is content as plain MARKDOWN; `categories` is an optional array of category ids. The slug is derived from the title (auto-deduped). Returns the created note including its new slug."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Note title." },
                "body": { "type": "string", "description": "Note content as markdown." },
                "categories": { "type": "array", "items": { "type": "string" }, "description": "Optional category ids to tag the note with." }
            },
            "required": ["title"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match self
            .0
            .create_note(str_arg(&args, "title"), str_arg(&args, "body"), category_ids(&args))
            .await
        {
            Ok(v) => Ok(ok(200, serde_json::to_value(v).unwrap_or(Value::Null))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mnote_update_note ────────────────────────────────────────────────────────
pub struct UpdateNote(NotesStore);
#[async_trait]
impl metalcraft::Tool for UpdateNote {
    fn name(&self) -> &str { "mnote_update_note" }
    fn description(&self) -> &str {
        "Update a note by `slug`. Only the fields you send change: `title`, `body` (markdown — REPLACES the whole body), and `categories` (an array of ids that REPLACES the note's whole tag set). Returns the updated note."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "slug": { "type": "string", "description": "Note slug to update." },
                "title": { "type": "string", "description": "New title." },
                "body": { "type": "string", "description": "New full markdown body (replaces the old)." },
                "categories": { "type": "array", "items": { "type": "string" }, "description": "Category ids that replace the note's whole tag set." }
            },
            "required": ["slug"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        let Some(slug) = str_arg(&args, "slug") else {
            return Ok(err(NotesError::bad_request("slug is required")));
        };
        let base_version = args.get("base_version").and_then(|v| v.as_i64());
        match self
            .0
            .update_note(
                slug,
                str_arg(&args, "title"),
                str_arg(&args, "body"),
                base_version,
                category_ids(&args),
                present(&args, "title"),
                present(&args, "body"),
            )
            .await
        {
            Ok((status, v)) => Ok(ok(status, serde_json::to_value(v).unwrap_or(Value::Null))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mnote_delete_note ────────────────────────────────────────────────────────
pub struct DeleteNote(NotesStore);
#[async_trait]
impl metalcraft::Tool for DeleteNote {
    fn name(&self) -> &str { "mnote_delete_note" }
    fn description(&self) -> &str {
        "Delete a note by `slug`. Irreversible — confirm the exact note (title) with the user before deleting."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "slug": { "type": "string", "description": "Note slug to delete." } },
            "required": ["slug"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        let Some(slug) = str_arg(&args, "slug") else {
            return Ok(err(NotesError::bad_request("slug is required")));
        };
        match self.0.delete_note(slug).await {
            Ok(()) => Ok(ok(204, json!({ "deleted": true }))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mnote_list_categories ────────────────────────────────────────────────────
pub struct ListCategories(NotesStore);
#[async_trait]
impl metalcraft::Tool for ListCategories {
    fn name(&self) -> &str { "mnote_list_categories" }
    fn description(&self) -> &str {
        "List the account's categories (color-coded tags — there are no folders). Each has id, name, color, created_at. A user has at most 12; defaults are home, work, personal."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn call(&self, _args: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match self.0.list_categories().await {
            Ok(v) => Ok(ok(200, serde_json::to_value(v).unwrap_or(Value::Null))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mnote_create_category ────────────────────────────────────────────────────
pub struct CreateCategory(NotesStore);
#[async_trait]
impl metalcraft::Tool for CreateCategory {
    fn name(&self) -> &str { "mnote_create_category" }
    fn description(&self) -> &str {
        "Create a category (a color-coded tag), e.g. 'ideas'. A unique color is assigned automatically. Fails with 409 if the name is taken or the account already has 12 categories. Returns the created category."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "name": { "type": "string", "description": "Category name (e.g. 'ideas')." } },
            "required": ["name"]
        })
    }
    async fn call(&self, args: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        let Some(name) = str_arg(&args, "name") else {
            return Ok(err(NotesError::bad_request("name is required")));
        };
        match self.0.create_category(name).await {
            Ok(v) => Ok(ok(200, serde_json::to_value(v).unwrap_or(Value::Null))),
            Err(e) => Ok(err(e)),
        }
    }
}
