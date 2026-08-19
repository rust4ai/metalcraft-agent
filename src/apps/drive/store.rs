//! Drive data-access: folder tree + file metadata in pod SQLite, file **bytes**
//! in the `blobs` primitive. Ported from the cloud `metalcraft-drive`, single-
//! user, with **direct upload** (bytes in on write) instead of the browser
//! presign→confirm dance — the pod is a backend, and the agent/clients hand it
//! bytes.

use std::sync::Arc;

use serde_json::json;
use sqlx::SqlitePool;

use super::models::*;
use super::util::{now_iso, uuid};
use super::{DrvError, DrvResult};
use crate::apps::{AppEventHub, BlobStore, OwnerIdentity};

#[derive(Clone)]
pub struct DriveStore {
    pool: SqlitePool,
    owner: OwnerIdentity,
    events: AppEventHub,
    blobs: Arc<dyn BlobStore>,
}

impl DriveStore {
    pub fn new(pool: SqlitePool, owner: OwnerIdentity, events: AppEventHub, blobs: Arc<dyn BlobStore>) -> Self {
        Self { pool, owner, events, blobs }
    }

    pub fn events(&self) -> &AppEventHub {
        &self.events
    }

    pub async fn ensure_ready(&self) -> DrvResult<()> {
        super::schema::apply(&self.pool).await
    }

    pub fn whoami(&self) -> serde_json::Value {
        json!({
            "sub": self.owner.user_id.clone().unwrap_or_else(|| "owner".to_string()),
            "email": self.owner.email,
            "scopes": serde_json::Value::Null,
        })
    }

    fn publish_file(&self, view: &FileView) {
        if let Ok(v) = serde_json::to_value(view) {
            self.events.publish(json!({ "type": "file.upserted", "file": v }));
        }
    }
    fn publish_file_deleted(&self, id: &str) {
        self.events.publish(json!({ "type": "file.deleted", "id": id }));
    }
    fn publish_folder(&self, view: &FolderView) {
        if let Ok(v) = serde_json::to_value(view) {
            self.events.publish(json!({ "type": "folder.upserted", "folder": v }));
        }
    }

    // ── folders ──────────────────────────────────────────────────────────────

    async fn folder_exists(&self, id: &str) -> DrvResult<bool> {
        let n: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM folders WHERE id = ?)")
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
        Ok(n != 0)
    }

    pub async fn create_folder(&self, name: &str, parent_id: Option<&str>) -> DrvResult<FolderView> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DrvError::bad_request("name is required"));
        }
        if let Some(p) = parent_id.filter(|s| !s.is_empty()) {
            if !self.folder_exists(p).await? {
                return Err(DrvError::not_found("parent folder not found"));
            }
        }
        let now = now_iso();
        let row = sqlx::query_as::<_, FolderRow>(
            "INSERT INTO folders (id, parent_id, name, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?) RETURNING *",
        )
        .bind(uuid())
        .bind(parent_id.filter(|s| !s.is_empty()))
        .bind(name)
        .bind(&now)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        let view = FolderView::from(row);
        self.publish_folder(&view);
        Ok(view)
    }

    /// Contents of a folder (its child folders + non-trashed files). `folder` of
    /// `None`/`"root"` is the drive root (parent/folder = NULL).
    pub async fn list_folder(&self, folder: Option<&str>) -> DrvResult<serde_json::Value> {
        let folder = folder.map(str::trim).filter(|s| !s.is_empty() && *s != "root");
        if let Some(f) = folder {
            if !self.folder_exists(f).await? {
                return Err(DrvError::not_found("folder not found"));
            }
        }
        let folders = if let Some(f) = folder {
            sqlx::query_as::<_, FolderRow>("SELECT * FROM folders WHERE parent_id = ? ORDER BY name")
                .bind(f)
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query_as::<_, FolderRow>("SELECT * FROM folders WHERE parent_id IS NULL ORDER BY name")
                .fetch_all(&self.pool)
                .await?
        };
        let files = if let Some(f) = folder {
            sqlx::query_as::<_, FileRow>(
                "SELECT * FROM files WHERE folder_id = ? AND trashed_at IS NULL ORDER BY name",
            )
            .bind(f)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, FileRow>(
                "SELECT * FROM files WHERE folder_id IS NULL AND trashed_at IS NULL ORDER BY name",
            )
            .fetch_all(&self.pool)
            .await?
        };
        Ok(json!({
            "folders": folders.into_iter().map(FolderView::from).collect::<Vec<_>>(),
            "files": files.into_iter().map(FileView::from).collect::<Vec<_>>(),
        }))
    }

    // ── files ────────────────────────────────────────────────────────────────

    async fn file_row(&self, id: &str) -> DrvResult<FileRow> {
        sqlx::query_as::<_, FileRow>("SELECT * FROM files WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| DrvError::not_found("file not found"))
    }

    pub async fn get_file(&self, id: &str) -> DrvResult<FileView> {
        Ok(FileView::from(self.file_row(id).await?))
    }

    /// Store `bytes` as a new file. Bytes go to the blob store; metadata to SQLite.
    pub async fn upload(
        &self,
        name: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        folder_id: Option<&str>,
    ) -> DrvResult<FileView> {
        let name = name.trim();
        if name.is_empty() {
            return Err(DrvError::bad_request("name is required"));
        }
        if let Some(f) = folder_id.filter(|s| !s.is_empty()) {
            if !self.folder_exists(f).await? {
                return Err(DrvError::not_found("folder not found"));
            }
        }
        let id = uuid();
        let blob_key = format!("files/{id}");
        let size = bytes.len() as i64;
        self.blobs
            .put(&blob_key, bytes)
            .await
            .map_err(|e| DrvError::new(500, format!("blob write failed: {e}")))?;
        let now = now_iso();
        let row = sqlx::query_as::<_, FileRow>(
            "INSERT INTO files (id, folder_id, name, blob_key, content_type, size_bytes, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING *",
        )
        .bind(&id)
        .bind(folder_id.filter(|s| !s.is_empty()))
        .bind(name)
        .bind(&blob_key)
        .bind(content_type.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("application/octet-stream"))
        .bind(size)
        .bind(&now)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        let view = FileView::from(row);
        self.publish_file(&view);
        Ok(view)
    }

    /// Fetch a file's bytes (+ its metadata) for download.
    pub async fn download(&self, id: &str) -> DrvResult<(FileView, Vec<u8>)> {
        let row = self.file_row(id).await?;
        let bytes = self
            .blobs
            .get(&row.blob_key)
            .await
            .map_err(|e| DrvError::new(500, format!("blob read failed: {e}")))?
            .ok_or_else(|| DrvError::not_found("file bytes missing"))?;
        Ok((FileView::from(row), bytes))
    }

    // ── public sharing (C4) ──────────────────────────────────────────────────

    /// Mark a file public and return its share token (idempotent).
    pub async fn share(&self, id: &str) -> DrvResult<String> {
        let row = self.file_row(id).await?;
        if let Some(t) = row.public_token {
            return Ok(t);
        }
        let token = uuid::Uuid::new_v4().simple().to_string();
        sqlx::query("UPDATE files SET public_token = ? WHERE id = ?")
            .bind(&token)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(token)
    }

    /// Remove a file's public link; returns the token that was cleared, if any.
    pub async fn unshare(&self, id: &str) -> DrvResult<Option<String>> {
        let row = self.file_row(id).await?;
        sqlx::query("UPDATE files SET public_token = NULL WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(row.public_token)
    }

    /// Resolve a share token to a downloadable file: `(content_type, name, bytes)`.
    pub async fn public_file(&self, token: &str) -> DrvResult<(String, String, Vec<u8>)> {
        let row = sqlx::query_as::<_, FileRow>(
            "SELECT * FROM files WHERE public_token = ? AND trashed_at IS NULL",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DrvError::not_found("not found"))?;
        let bytes = self
            .blobs
            .get(&row.blob_key)
            .await
            .map_err(|e| DrvError::new(500, format!("blob read failed: {e}")))?
            .ok_or_else(|| DrvError::not_found("file bytes missing"))?;
        Ok((row.content_type, row.name, bytes))
    }

    /// Rename / move / star / trash / restore (any provided field).
    pub async fn update_file(
        &self,
        id: &str,
        name: Option<&str>,
        folder_id: Option<Option<&str>>, // Some(None)=move to root, Some(Some(x))=move, None=leave
        starred: Option<bool>,
        trashed: Option<bool>,
    ) -> DrvResult<FileView> {
        self.file_row(id).await?; // 404
        if let Some(Some(f)) = folder_id {
            if !self.folder_exists(f).await? {
                return Err(DrvError::not_found("target folder not found"));
            }
        }
        let now = now_iso();
        // Build the update explicitly so we can express "move to root" (NULL).
        let name = name.map(str::trim).filter(|s| !s.is_empty());
        let trashed_at: Option<Option<String>> = trashed.map(|t| if t { Some(now.clone()) } else { None });
        // All plain `?` bound in order (mixing `?` and `?N` binds inconsistently
        // under sqlx-sqlite). The CASE WHEN <bool> gates conditional columns:
        // folder_id only changes on a move (NULL = move-to-root), trashed_at only
        // on a trash/restore.
        let row = sqlx::query_as::<_, FileRow>(
            "UPDATE files SET
               name       = COALESCE(?, name),
               folder_id  = CASE WHEN ? THEN ? ELSE folder_id END,
               starred    = COALESCE(?, starred),
               trashed_at = CASE WHEN ? THEN ? ELSE trashed_at END,
               updated_at = ?
             WHERE id = ? RETURNING *",
        )
        .bind(name) // name
        .bind(folder_id.is_some()) // move?
        .bind(folder_id.flatten()) // new folder (NULL if move-to-root)
        .bind(starred.map(|b| b as i64)) // starred
        .bind(trashed.is_some()) // trash change?
        .bind(trashed_at.flatten()) // trashed_at value
        .bind(&now) // updated_at
        .bind(id) // id
        .fetch_one(&self.pool)
        .await?;
        let view = FileView::from(row);
        self.publish_file(&view);
        Ok(view)
    }

    /// Delete: soft (trash) by default, or `permanent` (removes the blob + row).
    pub async fn delete_file(&self, id: &str, permanent: bool) -> DrvResult<()> {
        let row = self.file_row(id).await?;
        if permanent {
            let _ = self.blobs.delete(&row.blob_key).await; // best-effort blob cleanup
            sqlx::query("DELETE FROM files WHERE id = ?").bind(id).execute(&self.pool).await?;
            self.publish_file_deleted(id);
        } else {
            let updated =
                sqlx::query_as::<_, FileRow>("UPDATE files SET trashed_at = ?, updated_at = ? WHERE id = ? RETURNING *")
                    .bind(now_iso())
                    .bind(now_iso())
                    .bind(id)
                    .fetch_one(&self.pool)
                    .await?;
            self.publish_file(&FileView::from(updated));
        }
        Ok(())
    }

    pub async fn list_starred(&self) -> DrvResult<Vec<FileView>> {
        let rows = sqlx::query_as::<_, FileRow>(
            "SELECT * FROM files WHERE starred = 1 AND trashed_at IS NULL ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(FileView::from).collect())
    }

    pub async fn list_trash(&self) -> DrvResult<Vec<FileView>> {
        let rows = sqlx::query_as::<_, FileRow>(
            "SELECT * FROM files WHERE trashed_at IS NOT NULL ORDER BY trashed_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(FileView::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::LocalBlobStore;

    async fn store() -> DriveStore {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let blobs: Arc<dyn BlobStore> = Arc::new(LocalBlobStore::new(dir.path().to_path_buf()));
        // Keep the tempdir alive for the test by leaking it (test process only).
        std::mem::forget(dir);
        let s = DriveStore::new(pool, OwnerIdentity::default(), AppEventHub::new(), blobs);
        s.ensure_ready().await.unwrap();
        s
    }

    #[tokio::test]
    async fn upload_download_and_metadata() {
        let s = store().await;
        let f = s.upload("hello.txt", b"hi there".to_vec(), Some("text/plain"), None).await.unwrap();
        assert_eq!(f.size_bytes, 8);
        assert_eq!(f.content_type, "text/plain");
        let (view, bytes) = s.download(&f.id).await.unwrap();
        assert_eq!(view.name, "hello.txt");
        assert_eq!(bytes, b"hi there");
    }

    #[tokio::test]
    async fn share_public_download_and_unshare() {
        let s = store().await;
        let f = s.upload("report.pdf", b"%PDF-1.4 data".to_vec(), Some("application/pdf"), None).await.unwrap();
        let token = s.share(&f.id).await.unwrap();
        assert_eq!(token.len(), 32);
        // Idempotent — same token.
        assert_eq!(s.share(&f.id).await.unwrap(), token);

        let (ct, name, bytes) = s.public_file(&token).await.unwrap();
        assert_eq!(ct, "application/pdf");
        assert_eq!(name, "report.pdf");
        assert_eq!(bytes, b"%PDF-1.4 data");

        let cleared = s.unshare(&f.id).await.unwrap();
        assert_eq!(cleared.as_deref(), Some(token.as_str()));
        assert!(s.public_file(&token).await.is_err()); // token no longer resolves
    }

    #[tokio::test]
    async fn folders_and_listing() {
        let s = store().await;
        let folder = s.create_folder("Docs", None).await.unwrap();
        s.upload("root.txt", b"r".to_vec(), None, None).await.unwrap();
        s.upload("in-docs.txt", b"d".to_vec(), None, Some(&folder.id)).await.unwrap();

        let root = s.list_folder(None).await.unwrap();
        assert_eq!(root["folders"].as_array().unwrap().len(), 1);
        assert_eq!(root["files"].as_array().unwrap().len(), 1); // only root.txt
        let docs = s.list_folder(Some(&folder.id)).await.unwrap();
        assert_eq!(docs["files"].as_array().unwrap().len(), 1); // in-docs.txt
    }

    #[tokio::test]
    async fn star_trash_and_permanent_delete() {
        let s = store().await;
        let f = s.upload("a.bin", b"x".to_vec(), None, None).await.unwrap();
        s.update_file(&f.id, None, None, Some(true), None).await.unwrap();
        assert_eq!(s.list_starred().await.unwrap().len(), 1);

        // trash → leaves root listing, appears in trash
        s.delete_file(&f.id, false).await.unwrap();
        assert_eq!(s.list_folder(None).await.unwrap()["files"].as_array().unwrap().len(), 0);
        assert_eq!(s.list_trash().await.unwrap().len(), 1);
        // starred excludes trashed
        assert_eq!(s.list_starred().await.unwrap().len(), 0);

        // restore
        s.update_file(&f.id, None, None, None, Some(false)).await.unwrap();
        assert_eq!(s.list_trash().await.unwrap().len(), 0);

        // permanent delete removes it
        s.delete_file(&f.id, true).await.unwrap();
        assert!(s.get_file(&f.id).await.is_err());
    }

    #[tokio::test]
    async fn move_file_between_folders_and_root() {
        let s = store().await;
        let a = s.create_folder("A", None).await.unwrap();
        let f = s.upload("m.txt", b"m".to_vec(), None, Some(&a.id)).await.unwrap();
        // move to root
        let moved = s.update_file(&f.id, None, Some(None), None, None).await.unwrap();
        assert!(moved.folder_id.is_none());
        assert_eq!(s.list_folder(None).await.unwrap()["files"].as_array().unwrap().len(), 1);
    }
}
