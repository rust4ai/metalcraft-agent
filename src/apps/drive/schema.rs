//! Pod-local Drive schema — the single-user SQLite translation of the cloud
//! `metalcraft-drive` (migration 001), minus `owner_user_id` (one user per pod)
//! and the `pending_uploads` presign ledger (the pod uploads bytes directly to
//! its `blobs` store, so there's no presign→confirm gap to reconcile). App
//! filespaces (migration 002) are out of scope for v1.
//!
//! File **metadata** lives here; file **bytes** live in the `blobs` primitive at
//! `blob_key`.

use sqlx::SqlitePool;

use super::{DrvError, DrvResult};

pub const STATEMENTS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS folders (
       id         TEXT PRIMARY KEY,
       parent_id  TEXT REFERENCES folders(id) ON DELETE CASCADE,
       name       TEXT NOT NULL,
       created_at TEXT NOT NULL,
       updated_at TEXT NOT NULL
     )",
    "CREATE INDEX IF NOT EXISTS idx_folders_parent ON folders (parent_id)",
    "CREATE TABLE IF NOT EXISTS files (
       id           TEXT PRIMARY KEY,
       folder_id    TEXT REFERENCES folders(id) ON DELETE CASCADE,
       name         TEXT NOT NULL,
       blob_key     TEXT NOT NULL UNIQUE,
       content_type TEXT NOT NULL DEFAULT 'application/octet-stream',
       size_bytes   INTEGER NOT NULL DEFAULT 0,
       starred      INTEGER NOT NULL DEFAULT 0,
       trashed_at   TEXT,
       public_token TEXT UNIQUE,          -- set ⇒ shared via the coordinator
       created_at   TEXT NOT NULL,
       updated_at   TEXT NOT NULL
     )",
    "CREATE INDEX IF NOT EXISTS idx_files_folder  ON files (folder_id)",
    "CREATE INDEX IF NOT EXISTS idx_files_trashed ON files (trashed_at)",
];

pub async fn apply(pool: &SqlitePool) -> DrvResult<()> {
    for stmt in STATEMENTS {
        sqlx::query(stmt).execute(pool).await.map_err(DrvError::from)?;
    }
    Ok(())
}
