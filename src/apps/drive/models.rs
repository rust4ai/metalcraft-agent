//! Row + output shapes for Drive.

use serde::Serialize;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct FolderRow {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderView {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

impl From<FolderRow> for FolderView {
    fn from(r: FolderRow) -> Self {
        FolderView { id: r.id, parent_id: r.parent_id, name: r.name, created_at: r.created_at, updated_at: r.updated_at }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct FileRow {
    pub id: String,
    pub folder_id: Option<String>,
    pub name: String,
    /// Key into the `blobs` primitive where the bytes live (not serialized).
    pub blob_key: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub starred: i64,
    pub trashed_at: Option<String>,
    pub public_token: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileView {
    pub id: String,
    pub folder_id: Option<String>,
    pub name: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub starred: bool,
    pub trashed: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl From<FileRow> for FileView {
    fn from(r: FileRow) -> Self {
        FileView {
            id: r.id,
            folder_id: r.folder_id,
            name: r.name,
            content_type: r.content_type,
            size_bytes: r.size_bytes,
            starred: r.starred != 0,
            trashed: r.trashed_at.is_some(),
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}
