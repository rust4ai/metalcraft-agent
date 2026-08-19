//! The `mdrv_*` native tools. Local file I/O (`upload`/`download`) is jailed to
//! the upload-root sandbox, like the multipart/s3 tools.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::store::DriveStore;
use super::util::jailed_path;
use super::DrvError;

pub fn register(reg: metalcraft::ToolRegistry, store: DriveStore) -> metalcraft::ToolRegistry {
    reg.register(Whoami(store.clone()))
        .register(ListFolder(store.clone()))
        .register(CreateFolder(store.clone()))
        .register(UploadFile(store.clone()))
        .register(DownloadFile(store.clone()))
        .register(GetFile(store.clone()))
        .register(UpdateFile(store.clone()))
        .register(DeleteFile(store.clone()))
        .register(ListStarred(store.clone()))
        .register(ListTrash(store))
}

fn ok(status: u16, data: Value) -> Value {
    json!({ "status": status, "data": data })
}
fn err(e: DrvError) -> Value {
    json!({ "status": e.status, "data": { "error": e.message } })
}
async fn ready(s: &DriveStore) -> Result<(), Value> {
    s.ensure_ready().await.map_err(err)
}
fn sa<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(|v| v.as_str())
}
fn jval<T: serde::Serialize>(v: T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

// ── mdrv_whoami ──────────────────────────────────────────────────────────────
pub struct Whoami(DriveStore);
#[async_trait]
impl metalcraft::Tool for Whoami {
    fn name(&self) -> &str { "mdrv_whoami" }
    fn description(&self) -> &str { "Validate access and see identity/scope. Returns { sub, email, scopes }." }
    fn parameters_schema(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    async fn call(&self, _a: Value) -> metalcraft::Result<Value> { Ok(ok(200, self.0.whoami())) }
}

// ── mdrv_list_folder ─────────────────────────────────────────────────────────
pub struct ListFolder(DriveStore);
#[async_trait]
impl metalcraft::Tool for ListFolder {
    fn name(&self) -> &str { "mdrv_list_folder" }
    fn description(&self) -> &str {
        "List a folder's contents: { folders, files }. Pass `folder` (a folder id) or 'root'/omit for the drive root. Files exclude trashed ones."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "folder": { "type": "string", "description": "Folder id, or 'root'/omit for the root." } } })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match self.0.list_folder(sa(&a, "folder")).await {
            Ok(v) => Ok(ok(200, v)),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mdrv_create_folder ───────────────────────────────────────────────────────
pub struct CreateFolder(DriveStore);
#[async_trait]
impl metalcraft::Tool for CreateFolder {
    fn name(&self) -> &str { "mdrv_create_folder" }
    fn description(&self) -> &str { "Create a folder. `name` required; optional `parent_id` (omit for a root folder)." }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Folder name." },
                "parent_id": { "type": "string", "description": "Optional parent folder id (omit for root)." }
            },
            "required": ["name"]
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match self.0.create_folder(sa(&a, "name").unwrap_or(""), sa(&a, "parent_id")).await {
            Ok(v) => Ok(ok(200, jval(v))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mdrv_upload_file ─────────────────────────────────────────────────────────
pub struct UploadFile(DriveStore);
#[async_trait]
impl metalcraft::Tool for UploadFile {
    fn name(&self) -> &str { "mdrv_upload_file" }
    fn description(&self) -> &str {
        "Upload a local file into Drive. `file_path` is read from the upload sandbox; `name` is the stored name (defaults to the file's name). Optional `folder_id` and `content_type`. Returns the file metadata."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to the local file (relative to the upload root)." },
                "name": { "type": "string", "description": "Stored file name (defaults to the file's base name)." },
                "folder_id": { "type": "string", "description": "Optional destination folder id (omit for root)." },
                "content_type": { "type": "string", "description": "Optional MIME type." }
            },
            "required": ["file_path"]
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        let Some(fp) = sa(&a, "file_path") else {
            return Ok(err(DrvError::bad_request("file_path is required")));
        };
        let path = match jailed_path(fp) {
            Ok(p) => p,
            Err(m) => return Ok(err(DrvError::bad_request(m))),
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return Ok(err(DrvError::bad_request(format!("cannot read {fp}: {e}")))),
        };
        let name = sa(&a, "name")
            .map(str::to_string)
            .unwrap_or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into()));
        match self.0.upload(&name, bytes, sa(&a, "content_type"), sa(&a, "folder_id")).await {
            Ok(v) => Ok(ok(200, jval(v))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mdrv_download_file ───────────────────────────────────────────────────────
pub struct DownloadFile(DriveStore);
#[async_trait]
impl metalcraft::Tool for DownloadFile {
    fn name(&self) -> &str { "mdrv_download_file" }
    fn description(&self) -> &str {
        "Download a file's bytes to a local path in the upload sandbox. `id` is the file id; `dest_path` is where to write it. Returns { path, size_bytes }."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "File id (from mdrv_list_folder)." },
                "dest_path": { "type": "string", "description": "Local destination path (relative to the upload root)." }
            },
            "required": ["id", "dest_path"]
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        let (Some(id), Some(dest)) = (sa(&a, "id"), sa(&a, "dest_path")) else {
            return Ok(err(DrvError::bad_request("id and dest_path are required")));
        };
        let path = match jailed_path(dest) {
            Ok(p) => p,
            Err(m) => return Ok(err(DrvError::bad_request(m))),
        };
        match self.0.download(id).await {
            Ok((view, bytes)) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(&path, &bytes) {
                    return Ok(err(DrvError::new(500, format!("cannot write {dest}: {e}"))));
                }
                Ok(ok(200, json!({ "path": dest, "size_bytes": view.size_bytes })))
            }
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mdrv_get_file ────────────────────────────────────────────────────────────
pub struct GetFile(DriveStore);
#[async_trait]
impl metalcraft::Tool for GetFile {
    fn name(&self) -> &str { "mdrv_get_file" }
    fn description(&self) -> &str { "Get a file's metadata by id." }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": { "id": { "type": "string", "description": "File id." } }, "required": ["id"] })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match sa(&a, "id") {
            Some(id) => match self.0.get_file(id).await {
                Ok(v) => Ok(ok(200, jval(v))),
                Err(e) => Ok(err(e)),
            },
            None => Ok(err(DrvError::bad_request("id is required"))),
        }
    }
}

// ── mdrv_update_file ─────────────────────────────────────────────────────────
pub struct UpdateFile(DriveStore);
#[async_trait]
impl metalcraft::Tool for UpdateFile {
    fn name(&self) -> &str { "mdrv_update_file" }
    fn description(&self) -> &str {
        "Update a file: `name` (rename), `folder_id` (move; empty string = root), `starred` (bool), `trashed` (bool: true=trash, false=restore). Returns the updated file."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "File id." },
                "name": { "type": "string", "description": "New name." },
                "folder_id": { "type": "string", "description": "Move to this folder id; empty string moves to root." },
                "starred": { "type": "boolean", "description": "Star / unstar." },
                "trashed": { "type": "boolean", "description": "true = move to trash, false = restore." }
            },
            "required": ["id"]
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        let Some(id) = sa(&a, "id") else {
            return Ok(err(DrvError::bad_request("id is required")));
        };
        // folder_id present → move; empty string → root (None), else Some(id).
        let folder_id = a.get("folder_id").map(|v| {
            let s = v.as_str().unwrap_or("").trim();
            if s.is_empty() { None } else { Some(s) }
        });
        let starred = a.get("starred").and_then(|v| v.as_bool());
        let trashed = a.get("trashed").and_then(|v| v.as_bool());
        match self.0.update_file(id, sa(&a, "name"), folder_id, starred, trashed).await {
            Ok(v) => Ok(ok(200, jval(v))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mdrv_delete_file ─────────────────────────────────────────────────────────
pub struct DeleteFile(DriveStore);
#[async_trait]
impl metalcraft::Tool for DeleteFile {
    fn name(&self) -> &str { "mdrv_delete_file" }
    fn description(&self) -> &str {
        "Delete a file. By default moves it to trash (recoverable); pass `permanent: true` to erase it and its bytes irreversibly."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "File id." },
                "permanent": { "type": "boolean", "description": "true = erase permanently (default false = trash)." }
            },
            "required": ["id"]
        })
    }
    async fn call(&self, a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        let Some(id) = sa(&a, "id") else {
            return Ok(err(DrvError::bad_request("id is required")));
        };
        let permanent = a.get("permanent").and_then(|v| v.as_bool()).unwrap_or(false);
        match self.0.delete_file(id, permanent).await {
            Ok(()) => Ok(ok(204, json!({ "deleted": true, "permanent": permanent }))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mdrv_list_starred ────────────────────────────────────────────────────────
pub struct ListStarred(DriveStore);
#[async_trait]
impl metalcraft::Tool for ListStarred {
    fn name(&self) -> &str { "mdrv_list_starred" }
    fn description(&self) -> &str { "List starred (non-trashed) files." }
    fn parameters_schema(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    async fn call(&self, _a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match self.0.list_starred().await {
            Ok(v) => Ok(ok(200, jval(v))),
            Err(e) => Ok(err(e)),
        }
    }
}

// ── mdrv_list_trash ──────────────────────────────────────────────────────────
pub struct ListTrash(DriveStore);
#[async_trait]
impl metalcraft::Tool for ListTrash {
    fn name(&self) -> &str { "mdrv_list_trash" }
    fn description(&self) -> &str { "List trashed files (recoverable via mdrv_update_file trashed=false)." }
    fn parameters_schema(&self) -> Value { json!({ "type": "object", "properties": {} }) }
    async fn call(&self, _a: Value) -> metalcraft::Result<Value> {
        if let Err(e) = ready(&self.0).await { return Ok(e); }
        match self.0.list_trash().await {
            Ok(v) => Ok(ok(200, jval(v))),
            Err(e) => Ok(err(e)),
        }
    }
}
